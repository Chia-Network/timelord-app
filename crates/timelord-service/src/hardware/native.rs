//! Native process lifetime, stderr telemetry and supervised I/O.
use super::{
    NativeFailure, NativeFailureKind, TEMPERATURE_TIMEOUT_SECONDS, arguments,
    tuning::AutomaticOwner,
};
use crate::{Shared, automatic, runtime::RawState};
use anyhow::{Context, Result, bail};
use std::{process::Stdio, sync::Arc, time::Duration};
use timelord_core::{BoardConfig, ProfileMode};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch},
};

// Versioned native telemetry distinguishes a new rate baseline from real idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Recovery {
    pub engine: u8,
}
impl Recovery {
    pub(super) fn parse(line: &str) -> Option<Self> {
        let (_, message) = line.split_once("VDF ")?;
        let (engine, message) = message.split_once(": Restarting VDF at ")?;
        message.strip_suffix(" iters")?.parse::<u64>().ok()?;
        let engine = engine.parse::<u8>().ok()?;
        (engine < 3).then_some(Self { engine })
    }
}
fn raw_speed(line: &str) -> Option<(usize, f64, RawState)> {
    let (_, payload) = line.split_once("TLRAW 2 ")?;
    let mut parts = payload.split_whitespace();
    let engine = parts.next()?.parse::<usize>().ok()?;
    let ips = parts.next()?.parse::<f64>().ok()?;
    let state = match parts.next()? {
        "running" if ips >= 0. => RawState::Running,
        "sampling" if ips == -1. => RawState::Sampling,
        "idle" if ips == -1. => RawState::Idle,
        _ => return None,
    };
    (engine < 3 && ips.is_finite() && parts.next().is_none()).then_some((engine, ips, state))
}

fn temperature(line: &str) -> Option<(f64, f64)> {
    let (_, value) = line.split_once("ASIC Temp = ")?;
    let (temp, frequency) = value.split_once(" C; Frequency = ")?;
    let (frequency, _) = frequency.split_once(" MHz")?;
    let temperature: f64 = temp.trim().parse().ok()?;
    let frequency: f64 = frequency.trim().parse().ok()?;
    (temperature.is_finite() && frequency.is_finite()).then_some((temperature, frequency))
}
pub(super) async fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    if let Some(pid) = child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await;
    }
    if tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}
// Drain stderr while live control awaits acknowledgement, including recovery floods.
pub(super) struct ClientLog {
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    ready: Option<oneshot::Sender<()>>,
    initialization: Option<tokio::sync::MutexGuard<'static, ()>>,
    last_temperature: tokio::time::Instant,
    recent: std::collections::VecDeque<String>,
}
impl ClientLog {
    pub(super) fn new(stderr: tokio::process::ChildStderr, ready: oneshot::Sender<()>) -> Self {
        Self {
            lines: BufReader::new(stderr).lines(),
            ready: Some(ready),
            initialization: None,
            last_temperature: tokio::time::Instant::now(),
            recent: Default::default(),
        }
    }
    pub(super) async fn read(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
    ) -> Result<Option<Recovery>> {
        let Some(line) = self.lines.next_line().await? else {
            bail!(
                "ASIC client log closed; recent log: {}",
                self.recent.iter().cloned().collect::<Vec<_>>().join(" | ")
            );
        };
        self.recent.push_back(line.clone());
        if self.recent.len() > 8 {
            self.recent.pop_front();
        }
        tracing::info!(board=%board.id, "{line}");
        let mut state = shared.runtime.lock().expect("runtime");
        if let Some((engine, ips, activity)) = raw_speed(&line) {
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .raw_sample(engine, ips, activity, crate::unix_now());
        }
        if let Some(value) = line
            .split_once("Board voltage is now ")
            .and_then(|(_, s)| s.split_whitespace().next())
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        {
            state.boards.entry(board.id.clone()).or_default().voltage_v = Some(value);
        }
        if line.contains("ASIC Temp = ") && temperature(&line).is_none() {
            return Err(NativeFailure::new(
                NativeFailureKind::SensorUnavailable,
                "ASIC temperature telemetry unavailable",
            )
            .into());
        }
        if let Some((temp, freq)) = temperature(&line) {
            if temp <= 0. {
                return Err(NativeFailure::new(
                    NativeFailureKind::SensorUnavailable,
                    "ASIC temperature telemetry unavailable",
                )
                .into());
            }
            self.last_temperature = tokio::time::Instant::now();
            let hardware = state.boards.entry(board.id.clone()).or_default();
            hardware.temperature_unix = crate::unix_now();
            hardware.temperature_c = Some(temp);
            hardware.frequency_mhz = Some(freq);
            if hardware.state == "starting" || hardware.state == "connected" {
                hardware.state = "connected".into();
            }
            if temp >= board.temperature_limit_c {
                let first = !hardware.latched_fault;
                hardware.state = "thermal_stop".into();
                hardware.latched_fault = true;
                state.event(Some(board.id.clone()), "thermal_stop");
                if first {
                    shared
                        .process
                        .metrics
                        .thermal_stopped(&crate::observability::board(&shared.config, &board.id));
                }
                return Err(NativeFailure::new(
                    NativeFailureKind::Thermal,
                    "ASIC reached its configured temperature limit",
                )
                .into());
            }
            // Pinned native code emits this only from event_loop/read_hw_status,
            // after init_hw completed USB open, clock/voltage and PVT setup.
            self.initialization.take();
        }
        let recovery = Recovery::parse(&line);
        if let Some(event) = recovery {
            tracing::debug!(engine = event.engine, "Native engine recovered");
            state.boards.entry(board.id.clone()).or_default().recoveries += 1;
            state.event(Some(board.id.clone()), "recovery");
            shared.process.metrics.asic_recovered(
                &crate::observability::board(&shared.config, &board.id),
                event.engine,
            );
        }
        if line.contains("Connected to timelord, waiting for challenge") {
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .native_initialized = true;
            self.initialization.take();
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(());
            }
        }
        Ok(recovery)
    }
}

pub(super) async fn supervise(
    shared: Arc<Shared>,
    board: BoardConfig,
    mut child: Child,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    initialization: tokio::sync::MutexGuard<'static, ()>,
) -> Result<()> {
    let automatic = if board.profile == ProfileMode::Automatic {
        // Reuse prevalidated capabilities; owner setup includes learned state
        // and a control exchange with its own thirty-second acknowledgement limit.
        tokio::select! {
            result=tokio::time::timeout(Duration::from_secs(45), AutomaticOwner::new(&shared,&board,&mut child))=>Some(result.context("ASIC initialization timed out").and_then(|r| r)),
            _=shutdown.changed()=>{terminate(&mut child).await;return Ok(());}
        }
    } else {
        None
    };
    let mut automatic = match automatic {
        Some(Ok(owner)) => Some(owner),
        Some(Err(error)) => {
            terminate(&mut child).await;
            let mut state = shared.runtime.lock().expect("runtime");
            state.boards.entry(board.id.clone()).or_default().state = "fault".into();
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .fault(&error);
            return Err(error);
        }
        None => None,
    };
    let output = child.stderr.take().context("ASIC client log unavailable")?;
    let mut log = ClientLog::new(output, ready);
    // A live-control ACK is issued by event_loop after init_hw/init_vdf_client.
    // Socket/network readiness is a separate lifetime, not a USB-open lock.
    log.initialization = if automatic.is_some() {
        drop(initialization);
        None
    } else {
        Some(initialization)
    };
    // A successful setup gets its own bounded ready wait. A near-limit ACK
    // must not consume the budget for reading the subsequent ready marker.
    let initialization_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let result: Result<()> = loop {
        tokio::select! {
            _ = shutdown.changed() => break Ok(()),
            // Bound network readiness separately from completed USB initialization.
            _ = tokio::time::sleep_until(initialization_deadline), if log.ready.is_some() => {
                break Err(anyhow::anyhow!("ASIC initialization timed out"));
            }
            _ = watchdog.tick() => {
                if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get(&board.id)
                    && (hardware.latched_fault || hardware.state == "fault") {
                    break Err(anyhow::anyhow!(hardware.fault_reason.clone().unwrap_or_else(|| "ASIC needs attention".into())));
                }
                if log.last_temperature.elapsed() > Duration::from_secs(TEMPERATURE_TIMEOUT_SECONDS) { break Err(anyhow::anyhow!("ASIC temperature telemetry stopped")); }
                match child.try_wait() {
                    Ok(Some(exit)) => break Err(anyhow::anyhow!("ASIC client exited: {exit}")),
                    Err(error) => break Err(error.into()),
                    Ok(None) => {}
                }
                if let Some(owner)=&mut automatic && let Err(error)=owner.tick(&shared,&board,&mut log).await {break Err(error);}
            }
            result = log.read(&shared, &board) => {
                let recovery = match result { Ok(recovery) => recovery, Err(error) => break Err(error) };
                if recovery.is_some() && let Some(owner)=&mut automatic && let Err(error)=owner.tick(&shared,&board,&mut log).await {break Err(error);}
            }
        }
    };
    terminate(&mut child).await;
    let mut state = shared.runtime.lock().expect("runtime");
    let hardware = state.boards.entry(board.id.clone()).or_default();
    hardware.raw_speeds.clear();
    if hardware.state != "thermal_stop" {
        hardware.state = if result.is_ok() { "stopped" } else { "fault" }.into();
    }
    if let Err(error) = &result {
        state
            .boards
            .entry(board.id.clone())
            .or_default()
            .fault(error);
    }
    result
}

pub(super) async fn launch(shared: &Shared, board: &BoardConfig) -> Result<Child> {
    let engines = shared
        .assignments
        .iter()
        .filter(|a| a.board_id == board.id)
        .count();
    let mut args = arguments(board, engines)?;
    let automatic = board.profile == ProfileMode::Automatic;
    if automatic {
        let facts = shared
            .native
            .as_ref()
            .context("Automatic capabilities were not prepared")?;
        let saved = facts.learned(&shared.config, board, engines).await?;
        let point = automatic::initial_point(board, saved.as_ref());
        let frequency = args
            .iter()
            .position(|a| a == "--freq")
            .context("Automatic frequency missing")?
            + 1;
        let voltage = args
            .iter()
            .position(|a| a == "--voltage")
            .context("Automatic voltage missing")?
            + 1;
        args[frequency] = point.frequency_mhz.to_string();
        args[voltage] = point.voltage_v.to_string();
    }
    Command::new(
        shared
            .config
            .hardware
            .client_binary
            .as_ref()
            .context("No native client configured")?,
    )
    .args(args)
    .stdin(if automatic {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(if automatic {
        Stdio::piped()
    } else {
        Stdio::inherit()
    })
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .context("Launching native ASIC client")
}

#[cfg(test)]
mod tests;

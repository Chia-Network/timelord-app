//! One board's preparation, restart budget and recovery orchestration.
use super::{
    INITIALIZATION,
    native::{launch, supervise},
};
use crate::Shared;
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use timelord_core::BoardConfig;
use tokio::sync::{oneshot, watch};

const RESTART_DELAYS: [u64; 3] = [1, 5, 30];
pub(super) struct RestartBackoff {
    used: usize,
    delays: [Duration; 3],
}
impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            used: 0,
            delays: RESTART_DELAYS.map(Duration::from_secs),
        }
    }
}
impl RestartBackoff {
    fn next(&mut self, established: bool) -> Duration {
        if established {
            self.used = 0;
        }
        let delay = self.delays[self.used.min(self.delays.len() - 1)];
        self.used = self.used.saturating_add(1);
        delay
    }
}
fn established(shared: &Shared, board: &BoardConfig, started: u64, elapsed: Duration) -> bool {
    if elapsed < Duration::from_secs(600) {
        return false;
    }
    let now = crate::unix_now();
    let state = shared.runtime.lock().expect("runtime");
    let telemetry_fresh = state
        .boards
        .get(&board.id)
        .is_some_and(|b| now.saturating_sub(b.temperature_unix) <= 30);
    telemetry_fresh
        && shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .all(|a| {
                let proofs: Vec<_> = state
                    .observations
                    .iter()
                    .filter(|o| o.chain == a.chain && o.work_started_unix >= started)
                    .collect();
                proofs.len() >= 3
                    && proofs
                        .last()
                        .is_some_and(|o| now.saturating_sub(o.timestamp_unix) <= 30)
            })
}

fn thermal_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<super::NativeFailure>()
        .is_some_and(|e| e.kind == super::NativeFailureKind::Thermal)
}

pub(super) async fn supervise_board(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut board: BoardConfig,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    mut budget: RestartBackoff,
) {
    let mut ready = Some(ready);
    loop {
        if *shutdown.borrow() {
            return;
        }
        {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            if hardware.latched_fault {
                return;
            }
            hardware.generation += 1;
            hardware.started_unix = crate::unix_now();
            hardware.state = "starting".into();
            hardware.raw_speeds.clear();
            hardware.temperature_unix = 0;
            hardware.temperature_c = None;
            hardware.fault_reason = None;
            hardware.fault_kind = None;
        }
        let started = tokio::time::Instant::now();
        let started_unix = crate::unix_now();
        let tx = ready.take().unwrap_or_else(|| oneshot::channel().0);
        // D2XX enumeration and opening must not race another board's initialization.
        let initialization = tokio::select! {
            guard = INITIALIZATION.lock() => guard,
            _ = shutdown.changed() => return,
        };
        let preparation: Result<()> = tokio::select! {
            result = async {
                if board.serial.is_some() {
                    let devices = crate::inventory::discover(&shared.config).await?;
                    // This owner's old process has exited, so its own open FTDI
                    // handle cannot be hiding metadata. Only this observation
                    // may declare an already-managed board absent.
                    let matches = devices.iter().filter(|d| d.serial == board.serial).count();
                    shared.runtime.lock().expect("runtime").boards.get_mut(&board.id).expect("board").usb_present = match matches {
                        0 => Some(false), 1 => Some(true), _ => None,
                    };
                    board.device_location = Some(crate::inventory::board_location(&board, &devices)?);
                }
                shared.runtime.lock().expect("runtime").boards.get_mut(&board.id).expect("board").device_location = board.device_location;
                Ok(())
            } => result,
            _ = shutdown.changed() => return,
        };
        let result = match preparation {
            Ok(()) => match launch(&shared, &board).await {
                Ok(child) => {
                    supervise(
                        shared.clone(),
                        board.clone(),
                        child,
                        tx,
                        shutdown.clone(),
                        initialization,
                    )
                    .await
                }
                Err(error) => {
                    drop(tx);
                    drop(initialization);
                    Err(error)
                }
            },
            Err(error) => {
                drop(tx);
                drop(initialization);
                Err(error)
            }
        };
        if *shutdown.borrow() || result.is_ok() {
            return;
        }
        let error = result.unwrap_err();
        let delay = budget.next(established(
            &shared,
            &board,
            started_unix,
            started.elapsed(),
        ));
        let stop = {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            // Missing USB/sensor data is recoverable. Explicit overtemperature and
            // invalid verified proofs remain latched. Old ambiguous guards fail closed.
            let thermal = thermal_rejection(&error);
            hardware.latched_fault |= thermal;
            hardware.state = if thermal || hardware.state == "thermal_stop" {
                "thermal_stop"
            } else if hardware.latched_fault {
                "fault"
            } else {
                "reconnecting"
            }
            .into();
            hardware.raw_speeds.clear();
            hardware.temperature_c = None;
            hardware.temperature_unix = 0;
            hardware.fault(&error);
            let stop = hardware.latched_fault;
            if !state
                .boards
                .iter()
                .any(|(id, b)| id != &board.id && b.state == "connected")
            {
                state.phase = "waiting_for_clients".into();
                state.message = if stop {
                    format!("{} needs attention: {error}", board.name)
                } else {
                    format!("Recovering {}: {error}", board.name)
                };
            }
            if !state.events.iter().any(|e| {
                e.board_id.as_ref() == Some(&board.id)
                    && e.kind == "board_fault"
                    && e.end_unix.is_none()
            }) {
                state.event(Some(board.id.clone()), "board_fault");
            }
            stop
        };
        backend.stop_board(&board.id).await;
        tracing::warn!(board=%board.name, %error, attempt=budget.used, retry_seconds=delay.as_secs(), stop, "ASIC client stopped; other boards remain supervised");
        if stop {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests;

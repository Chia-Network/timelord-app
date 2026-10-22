//! Supervise only explicitly configured native clients. Initialization is sequential.
use crate::{
    Shared,
    automatic::{self, Action, Point, Policy},
    runtime::HardwareState,
};
use anyhow::{Context, Result, bail};
use std::{process::Stdio, sync::Arc, time::Duration};
use timelord_core::{BoardConfig, Config, ProfileMode};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch},
    task::JoinSet,
};

pub fn arguments(board: &BoardConfig, engines: usize) -> Result<Vec<String>> {
    let mut arguments = vec![
        "--ip".into(),
        "127.0.0.1".into(),
        "--device-loc".into(),
        board
            .device_location
            .expect("validated location")
            .to_string(),
        board.client_port.to_string(),
        engines.to_string(),
    ];
    let (frequency, voltage) = board.operating_point();
    arguments.extend([
        "--vdf-threads".into(),
        "4".into(),
        "--proof-threads".into(),
        "3".into(),
        "--checkpoint-threshold".into(),
        "250000".into(),
        "--freq".into(),
        frequency.to_string(),
        "--voltage".into(),
        voltage.to_string(),
    ]);
    if board.profile == ProfileMode::Automatic {
        arguments.push("--live-control".into());
    }
    Ok(arguments)
}
fn temperature(line: &str) -> Option<(f64, f64)> {
    let (_, value) = line.split_once("ASIC Temp = ")?;
    let (temp, frequency) = value.split_once(" C; Frequency = ")?;
    let (frequency, _) = frequency.split_once(" MHz")?;
    let temperature: f64 = temp.trim().parse().ok()?;
    let frequency: f64 = frequency.trim().parse().ok()?;
    (temperature.is_finite() && frequency.is_finite()).then_some((temperature, frequency))
}
async fn terminate(child: &mut Child) {
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
struct AutomaticOwner {
    policy: Policy,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    sequence: u64,
    serial: String,
    engines: usize,
    hash: String,
    host: String,
}
impl AutomaticOwner {
    async fn new(shared: &Shared, board: &BoardConfig, child: &mut Child) -> Result<Self> {
        let engines = shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .count();
        let hash = automatic::fingerprint(&shared.config)?;
        let host = automatic::host();
        let point = automatic::initial(&shared.config, board, engines, &hash, &host)?;
        let mut owner = Self {
            policy: Policy::new(board.automatic.clone(), point, crate::unix_now()),
            input: child.stdin.take().context("Native control input missing")?,
            output: BufReader::new(
                child
                    .stdout
                    .take()
                    .context("Native control output missing")?,
            ),
            sequence: 0,
            serial: board
                .serial
                .clone()
                .context("Automatic tuning needs a persistent board serial")?,
            engines,
            hash,
            host,
        };
        if let Some(entry) =
            automatic::learned(&shared.config, board, engines, &owner.hash, &owner.host)?
        {
            owner.policy.restore(&entry);
        }
        let actual = owner.change(point, 30).await?;
        owner.policy.applied(
            actual,
            crate::unix_now(),
            "Observing live proofs at the initial setting".into(),
        );
        Ok(owner)
    }
    async fn change(&mut self, target: Point, seconds: u64) -> Result<Point> {
        if !self
            .policy
            .settings
            .contains(target.frequency_mhz, target.voltage_v)
        {
            bail!("Automatic request exceeds configured limits");
        }
        self.sequence += 1;
        let sequence = self.sequence;
        tokio::time::timeout(Duration::from_secs(seconds), async {
            self.input
                .write_all(
                    format!(
                        "TLSET 1 {sequence} {:.6} {:.6}\n",
                        target.frequency_mhz, target.voltage_v
                    )
                    .as_bytes(),
                )
                .await?;
            self.input.flush().await?;
            let mut line = String::new();
            for _ in 0..16 {
                line.clear();
                if (&mut self.output).take(513).read_line(&mut line).await? == 0 {
                    bail!("Native control closed before acknowledgement");
                }
                if line.len() > 512 {
                    bail!("Native control response too large");
                }
                if line.starts_with("TLERR ") {
                    let parts: Vec<_> = line.trim_end().splitn(4, ' ').collect();
                    if parts.len() != 4 || parts[1] != "1" || parts[2].parse::<u64>()? != sequence {
                        bail!("Native control rejection does not match request");
                    }
                    bail!("Native control rejected request {sequence}: {}", parts[3]);
                }
                if !line.starts_with("TLACK ") {
                    continue;
                }
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() != 5 || parts[1] != "1" || parts[2].parse::<u64>()? != sequence {
                    bail!("Native control acknowledgement does not match request");
                }
                let actual = Point {
                    frequency_mhz: parts[3].parse()?,
                    voltage_v: parts[4].parse()?,
                };
                if !self
                    .policy
                    .settings
                    .contains(actual.frequency_mhz, actual.voltage_v)
                    || (actual.frequency_mhz - target.frequency_mhz).abs() > 1.
                    || (actual.voltage_v - target.voltage_v).abs() > 0.0063
                {
                    bail!("Native hardware readback does not match the requested setting");
                }
                return Ok(actual);
            }
            bail!("Native client did not acknowledge live control")
        })
        .await
        .context("Native live control timed out")?
    }
    fn record(&self, shared: &Shared, clean: u64) -> Result<()> {
        let path = shared
            .config
            .hardware
            .automatic_state
            .as_ref()
            .context("Automatic state path missing")?;
        automatic::save(
            path,
            automatic::Learned {
                serial: self.serial.clone(),
                engines: self.engines,
                host: self.host.clone(),
                client_sha256: self.hash.clone(),
                point: self.policy.point,
                active_seconds: clean,
                updated_unix: crate::unix_now(),
                ceiling_mhz: self.policy.learned_limits().0,
                cooldown_seconds: self.policy.learned_limits().1,
                voltage_rejected: self.policy.learned_limits().2,
            },
        )
    }
    async fn tick(&mut self, shared: &Shared, board: &BoardConfig) -> Result<()> {
        let now = crate::unix_now();
        let input = {
            let state = shared.runtime.lock().expect("runtime");
            let hardware = state
                .boards
                .get(&board.id)
                .context("Automatic board state missing")?;
            let chains: Vec<_> = shared
                .assignments
                .iter()
                .filter(|a| a.board_id == board.id)
                .map(|a| a.chain)
                .collect();
            let latest = state
                .observations
                .iter()
                .rev()
                .find(|o| chains.contains(&o.chain))
                .map(|o| o.timestamp_unix);
            let score = automatic::phase_score(
                &state.observations,
                &chains,
                self.policy.epoch,
                self.policy.settings.minimum_proofs_per_chain,
            );
            automatic::Input {
                healthy: state.phase == "running"
                    && ["ready", "authenticated"].contains(&state.node.as_str())
                    && latest.is_some_and(|t| now.saturating_sub(t) <= 30)
                    && now.saturating_sub(hardware.temperature_unix) <= 30,
                recoveries: hardware.recoveries,
                score,
                temperature: hardware.temperature_c,
                temperature_limit: board.temperature_limit_c,
                paused: state.tuning_paused,
            }
        };
        match self.policy.observe(now, input) {
            Some(Action::Stop(reason)) => bail!(reason),
            Some(Action::Save) => self.record(
                shared,
                self.policy
                    .status
                    .active_seconds
                    .max(self.policy.settings.healthy_seconds),
            )?,
            Some(Action::Set(point, reason)) => {
                let persist = !self.policy.in_trial();
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, %reason, "Applying automatic setting");
                let actual = self
                    .change(point, 5)
                    .await
                    .map_err(|error| anyhow::anyhow!("{}: {error}", board.name))?;
                self.policy.applied(actual, crate::unix_now(), reason);
                // Save rollback limits/cooldown, never an outstanding experiment.
                // Zero clean seconds keeps the resumed point explicitly provisional.
                if persist {
                    self.record(shared, 0)?;
                }
                let mut state = shared.runtime.lock().expect("runtime");
                state.event(Some(board.id.clone()), "profile_change");
                let hardware = state.boards.entry(board.id.clone()).or_default();
                hardware.frequency_mhz = Some(actual.frequency_mhz);
                hardware.voltage_v = Some(actual.voltage_v);
            }
            None => {}
        }
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(board.id.clone())
            .or_default()
            .automatic = Some(self.policy.status.clone());
        Ok(())
    }
}

async fn supervise(
    shared: Arc<Shared>,
    board: BoardConfig,
    mut child: Child,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let automatic = if board.profile == ProfileMode::Automatic {
        tokio::select! {
            result=AutomaticOwner::new(&shared,&board,&mut child)=>Some(result),
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
            state.phase = "fault".into();
            state.message = error.to_string();
            return Err(error);
        }
        None => None,
    };
    let output = child.stderr.take().context("ASIC client log unavailable")?;
    let mut lines = BufReader::new(output).lines();
    let mut ready = Some(ready);
    let mut last_temperature = tokio::time::Instant::now();
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let mut recent = std::collections::VecDeque::new();
    let result: Result<()> = loop {
        tokio::select! {
            _ = shutdown.changed() => break Ok(()),
            _ = watchdog.tick() => {
                if shared.runtime.lock().expect("runtime").phase == "fault" { break Err(anyhow::anyhow!("Timelord needs attention")); }
                if last_temperature.elapsed() > Duration::from_secs(60) { break Err(anyhow::anyhow!("ASIC temperature telemetry stopped")); }
                if let Some(exit)=child.try_wait()? { break Err(anyhow::anyhow!("ASIC client exited: {exit}")); }
                if let Some(owner)=&mut automatic && let Err(error)=owner.tick(&shared,&board).await {break Err(error);}
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    let exit=tokio::time::timeout(Duration::from_secs(1),child.wait()).await;
                    break Err(anyhow::anyhow!("ASIC client exited ({exit:?}); recent log: {}",recent.iter().cloned().collect::<Vec<String>>().join(" | ")));
                };
                recent.push_back(line.clone());if recent.len()>8 {recent.pop_front();}
                tracing::info!(board=%board.id, "{line}");
                {
                let mut state = shared.runtime.lock().expect("runtime");
                if let Some(value)=line.split_once("Board voltage is now ").and_then(|(_,s)|s.split_whitespace().next()).and_then(|v|v.parse::<f64>().ok()).filter(|v|v.is_finite()) { state.boards.entry(board.id.clone()).or_default().voltage_v=Some(value); }
                if let Some((temp, freq)) = temperature(&line) {
                    last_temperature = tokio::time::Instant::now();
                    let status = state.boards.entry(board.id.clone()).or_default();
                    status.temperature_unix=crate::unix_now();status.temperature_c = Some(temp); status.frequency_mhz = Some(freq); status.state = "connected".into();
                    if temp >= board.temperature_limit_c {
                        status.state = "thermal_stop".into(); state.event(Some(board.id.clone()), "thermal_stop");
                        break Err(anyhow::anyhow!("ASIC reached its configured temperature limit"));
                    }
                }
                if line.contains("Restarting VDF at ") { state.boards.entry(board.id.clone()).or_default().recoveries += 1; state.event(Some(board.id.clone()), "recovery"); }
                if line.contains("Connected to timelord, waiting for challenge") && let Some(ready) = ready.take() { let _ = ready.send(()); }
                }
                if line.contains("Restarting VDF at ") && let Some(owner)=&mut automatic && let Err(error)=owner.tick(&shared,&board).await {break Err(error);}
            }
        }
    };
    terminate(&mut child).await;
    let mut state = shared.runtime.lock().expect("runtime");
    let hardware = state.boards.entry(board.id.clone()).or_default();
    if hardware.state != "thermal_stop" {
        hardware.state = if result.is_ok() { "stopped" } else { "fault" }.into();
    }
    if let Err(error) = &result {
        state.message = format!("{}: {error}", board.name);
        state.phase = "fault".into();
    }
    result
}
pub async fn validate_automatic(config: &Config) -> Result<()> {
    if !config
        .hardware
        .boards
        .iter()
        .any(|b| b.profile == ProfileMode::Automatic)
    {
        return Ok(());
    }
    let binary = config
        .hardware
        .client_binary
        .as_ref()
        .context("Automatic mode requires a native client")?;
    let probe = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(binary)
            .arg("--control-version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Native control capability check timed out")??;
    if !probe.status.success() || probe.stdout != b"timelord-live-control-v1\n" {
        bail!(
            "Automatic mode needs the bundled live-control native client. Select it in board settings, or use Manual settings."
        );
    }
    let hash = automatic::fingerprint(config)?;
    let host = automatic::host();
    for board in &config.hardware.boards {
        if board.profile == ProfileMode::Automatic {
            if board.serial.as_ref().is_none_or(|s| s.is_empty()) {
                bail!(
                    "Automatic mode needs a persistent serial for {}",
                    board.name
                );
            }
            automatic::initial(
                config,
                board,
                config
                    .assignments()
                    .map_err(anyhow::Error::msg)?
                    .iter()
                    .filter(|a| a.board_id == board.id)
                    .count(),
                &hash,
                &host,
            )?;
        }
    }
    Ok(())
}

pub async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let binary = shared
        .config
        .hardware
        .client_binary
        .as_ref()
        .context("No native client configured")?;
    validate_automatic(&shared.config).await?;
    let mut children = JoinSet::new();
    let (stop, child_shutdown) = watch::channel(false);
    let result: Result<()> = async {
        for board in &shared.config.hardware.boards {
            let engines = shared.assignments.iter().filter(|a| a.board_id == board.id).count(); if engines == 0 { continue; }
            shared.runtime.lock().expect("runtime").boards.insert(board.id.clone(), HardwareState { state: "starting".into(), ..Default::default() });
            let is_automatic=board.profile==ProfileMode::Automatic;
            let mut args=arguments(board,engines)?;
            if is_automatic {
                let point=automatic::initial(&shared.config,board,engines,&automatic::fingerprint(&shared.config)?,&automatic::host())?;
                let frequency=args.iter().position(|a|a=="--freq").context("Automatic frequency missing")?+1;
                let voltage=args.iter().position(|a|a=="--voltage").context("Automatic voltage missing")?+1;
                args[frequency]=point.frequency_mhz.to_string();args[voltage]=point.voltage_v.to_string();
            }
            let child = Command::new(binary).args(args).stdin(if is_automatic{Stdio::piped()}else{Stdio::null()}).stdout(if is_automatic{Stdio::piped()}else{Stdio::inherit()}).stderr(Stdio::piped()).kill_on_drop(true).spawn().context("Launching native ASIC client")?;
            let (tx, rx) = oneshot::channel(); children.spawn(supervise(shared.clone(), board.clone(), child, tx, child_shutdown.clone()));
            tokio::select! { _ = shutdown.changed() => return Ok(()), Some(result) = children.join_next(), if !children.is_empty() => { result??; bail!("ASIC stopped during initialization"); }, initialized = tokio::time::timeout(Duration::from_secs(30), rx) => { initialized.context("ASIC initialization timed out")?.context("ASIC initialization failed")?; } }
        }
        tokio::select! { _ = shutdown.changed() => {}, Some(result) = children.join_next(), if !children.is_empty() => { result??; bail!("An ASIC client stopped"); } }
        Ok(())
    }.await;
    let _ = stop.send(true);
    while let Some(result) = children.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "ASIC shutdown failed");
        }
    }
    if let Err(error) = &result {
        let mut state = shared.runtime.lock().expect("runtime");
        state.phase = "fault".into();
        state.message = error.to_string();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_control_requires_matching_acknowledgements_and_keeps_hard_limits() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("tl-ack-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let binary = directory.join("fake-client");
        std::fs::write(&binary,"#!/bin/sh\nwhile read command version id frequency voltage; do\n  if [ \"$id\" = 4 ]; then printf '%0600d\\n' 0; continue; fi\n  if [ \"$id\" = 3 ]; then id=999; fi\n  if [ \"$id\" = 5 ]; then printf 'TLERR 1 5 readback frequency=0.000000\\n'; continue; fi\n  printf 'TLACK 1 %s %s %s\\n' \"$id\" \"$frequency\" \"$voltage\"\ndone\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config:Config=toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=4113\nserial='a'\nprofile='automatic'").unwrap();
        config.hardware.client_binary = Some(binary.clone());
        config.hardware.automatic_state = Some(directory.join("cache.json"));
        let board = config.hardware.boards[0].clone();
        let args = arguments(&board, 3).unwrap();
        assert!(args.windows(2).any(|a| a == ["--freq", "1450"]));
        assert!(args.windows(2).any(|a| a == ["--voltage", "1"]));
        assert!(args.contains(&"--live-control".into()));
        let shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
        };
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owner = AutomaticOwner::new(&shared, &board, &mut child)
            .await
            .unwrap();
        assert_eq!(owner.policy.point.frequency_mhz, 1450.);
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1700.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .is_err()
        );
        assert_eq!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1460.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap()
                .frequency_mhz,
            1460.
        );
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1470.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1480.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1490.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("readback frequency=0.000000")
        );
        terminate(&mut child).await;
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn automatic_and_manual_arguments_share_the_optimized_proof_settings() {
        let mut board: BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\ndevice_location=1").unwrap();
        let automatic = arguments(&board, 3).unwrap();
        assert!(automatic.contains(&"--live-control".into()));
        assert!(automatic.windows(2).any(|w| w == ["--freq", "1450"]));
        board.profile = ProfileMode::Manual;
        board.frequency_mhz = 1540.;
        let manual = arguments(&board, 1).unwrap();
        assert!(!manual.contains(&"--live-control".into()));
        assert!(manual.windows(2).any(|w| w == ["--freq", "1540"]));
        assert!(
            manual
                .windows(2)
                .any(|w| w == ["--checkpoint-threshold", "250000"])
        );
    }

    #[tokio::test]
    async fn temperature_guard_stops_only_the_owned_fixture() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "timelord-client-fixture-{}-{}",
            std::process::id(),
            crate::unix_now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let binary = directory.join("hw_vdf_client");
        std::fs::write(&binary, "#!/bin/sh\ntrap 'exit 0' TERM INT\nprintf '%s\\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' 'ASIC Temp = 55.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2\nwhile :; do sleep 1; done\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: Config = toml::from_str(
            "[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=12305\nprofile='manual'\n",
        )
        .unwrap();
        config.hardware.client_binary = Some(binary);
        let shared = Arc::new(Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Arc::new(std::sync::Mutex::new(Default::default())),
            runtime: std::sync::Mutex::new(Default::default()),
            started_unix: crate::unix_now(),
        });
        let (_stop, receiver) = watch::channel(false);
        let result = tokio::time::timeout(Duration::from_secs(5), run(shared.clone(), receiver))
            .await
            .unwrap();
        assert!(result.is_err());
        assert_eq!(
            shared.runtime.lock().unwrap().boards["a"].state,
            "thermal_stop"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn temperature_parser_rejects_nonfinite_measurements() {
        assert_eq!(
            temperature("2026 ASIC Temp = 39.90 C; Frequency = 1540.0 MHz; freq_idx = 1"),
            Some((39.9, 1540.0))
        );
        assert_eq!(
            temperature("ASIC Temp = NaN C; Frequency = 1540.0 MHz"),
            None
        );
    }
}

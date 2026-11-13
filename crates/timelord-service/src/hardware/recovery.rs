//! Current-state recovery for a persistent native owner. No offline stop/start gate.
use super::*;

pub(super) async fn establish(
    shared: &Shared,
    board: &BoardConfig,
    owner: &mut NativeOwner,
    log: &mut ClientLog,
    shutdown: &mut watch::Receiver<bool>,
    resume: Option<Point>,
) -> Result<bool> {
    let store = shared.runtime.lock().expect("runtime").safety.clone();
    let thermal = store
        .as_ref()
        .is_some_and(|s| s.policy(board).restriction == Some(crate::safety::Reason::Thermal));
    let invalid_proof = store
        .as_ref()
        .is_some_and(|s| s.policy(board).restriction == Some(crate::safety::Reason::InvalidProof));
    let limit = if thermal {
        (board.temperature_limit_c - 5.).min(50.)
    } else {
        board.temperature_limit_c
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if *shutdown.borrow() { return Ok(false); }
            let cool = {
                let state = shared.runtime.lock().expect("runtime");
                state.boards.get(&board.id).is_some_and(|h| h.temperature_c.is_some_and(|t| t > 0. && t < limit)
                    && crate::unix_now().saturating_sub(h.temperature_unix) <= 10)
            };
            if cool { break; }
            tokio::select! { _ = shutdown.changed() => return Ok(false), result = log.read(shared, board) => { result?; } }
        }
        if let Some(point) = resume {
            if let (Some(store), Some(serial)) = (&store, board.serial.as_deref()) {
                let token = store.owner_token(serial).context("Recovery owner lost its journal token")?;
                anyhow::ensure!(store.prepare_recovery_point(serial,token,point)?, "Recovery owner was superseded");
            }
            let actual = owner.control.change_with_logs(point,30,log,shared,board).await?;
            if let Some(tuning) = &mut owner.tuning { tuning.policy.applied(actual,crate::unix_now(),"Resuming after fresh cool hardware telemetry".into()); }
        }
        if invalid_proof {
            let backend = shared.runtime.lock().expect("runtime").prover.upgrade().context("Recovery prover is unavailable")?;
            let check = backend.check_board(&board.id);
            tokio::pin!(check);
            loop {
                tokio::select! {
                    result = &mut check => { result?; break; },
                    _ = shutdown.changed() => return Ok(false),
                    result = log.read(shared,board) => { result?; }
                }
            }
        }
        if let (Some(store), Some(serial)) = (&store, board.serial.as_deref()) {
            let token = store.owner_token(serial).context("Live owner lost its journal token")?;
            anyhow::ensure!(store.recovered(serial, token)?, "Live owner was superseded during recovery");
        }
        let mut state = shared.runtime.lock().expect("runtime");
        let hardware = state.boards.entry(board.id.clone()).or_default();
        hardware.lifecycle.recovered();
        hardware.fault_kind = None;
        hardware.recovery_reason = None;
        hardware.retry_after_unix = None;
        Ok(true)
    }).await.context("Waiting for fresh cool ASIC telemetry")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    #[tokio::test]
    async fn conservative_recovery_can_run_below_optimization_minima() {
        let directory =
            std::env::temp_dir().join(format!("tl-recovery-minima-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(crate::safety::Store::open(&directory.join("config.toml")).unwrap());
        let board: BoardConfig = toml::from_str("id='probe'\nname='Probe'\nserial='probe'\nprofile='automatic'\nclient_port=18110\n[automatic]\nmin_frequency_mhz=1600.0\nmin_voltage_v=0.9").unwrap();
        store
            .quarantine("probe", crate::safety::Reason::Thermal)
            .unwrap();
        let mut runtime = crate::runtime::RuntimeState::new(false);
        runtime.routing = Some(vec![]);
        runtime.safety = Some(store);
        let shared = Shared {
            config: Config::default(),
            assignments: vec![],
            clients: Default::default(),
            native: Some(Arc::new(Facts {
                hash: "a".repeat(64),
                host: "fixture".into(),
                frequencies: vec![200., 1450., 1600.],
            })),
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            started_unix: 0,
            service_started_unix: 0,
        };
        let startup = startup(&shared, &board).await.unwrap();
        assert_eq!(startup.point.frequency_mhz, 200.);
        assert_eq!(startup.point.voltage_v, 0.7);
        assert!(
            admission::qualification(&shared, &board, 1)
                .unwrap()
                .point
                .frequency_mhz
                <= 1450.
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    async fn fixture(thermal: bool, temperature: u32) {
        let directory = std::env::temp_dir().join(format!(
            "tl-warm-owner-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let helper = directory.join("client");
        let commands = directory.join("commands");
        std::fs::write(&helper,format!(r#"#!/bin/sh
while read command version id frequency voltage; do
 printf '%s\n' "$command $id $frequency $voltage" >> '{}'
 if [ "$command" = TLSTOP ]; then
  printf 'TLSTOPACK 1 %s 1 1\n' "$id"
  exit 0
 fi
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
 printf '%s\n' 'Connected to timelord, waiting for challenge' 'ASIC Temp = {temperature} C; Frequency = 1450 MHz' >&2
done
"#,commands.display())).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::default();
        config.hardware.boards.push(toml::from_str("id='warm'\nname='Warm'\nserial='warm-fixture'\ndevice_location=1\nprofile='manual'\nclient_port=18110\nfrequency_mhz=1450.0\nvoltage_v=1.0").unwrap());
        config.hardware.client_binary = Some(helper.clone());
        let board = config.hardware.boards[0].clone();
        let store = Arc::new(crate::safety::Store::open(&directory.join("config.toml")).unwrap());
        store
            .quarantine(
                "warm-fixture",
                if thermal {
                    crate::safety::Reason::Thermal
                } else {
                    crate::safety::Reason::UncleanOwner
                },
            )
            .unwrap();
        let mut runtime = crate::runtime::RuntimeState::new(false);
        runtime.routing = Some(config.assignments().unwrap());
        runtime.safety = Some(store.clone());
        runtime
            .boards
            .entry(board.id.clone())
            .or_default()
            .lifecycle
            .restore_policy(store.policy(&board), crate::unix_now());
        let shared = Arc::new(Shared {
            assignments: config.assignments().unwrap(),
            config,
            native: None,
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let startup = startup(&shared, &board).await.unwrap();
        store.begin_owner("warm-fixture", startup.point).unwrap();
        let child = Command::new(&helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let (ready, _) = oneshot::channel();
        let (stop, rx) = watch::channel(false);
        let initialization: &'static tokio::sync::Mutex<()> =
            Box::leak(Box::new(tokio::sync::Mutex::new(())));
        let task = tokio::spawn(supervise(
            shared.clone(),
            board.clone(),
            child,
            ready,
            rx,
            initialization.lock().await,
            None,
            startup,
        ));
        if temperature >= 55 {
            let result = tokio::time::timeout(Duration::from_secs(8), task)
                .await
                .unwrap()
                .unwrap();
            assert!(result.is_err());
            assert_eq!(
                store.board("warm-fixture").unwrap().quarantine,
                Some(crate::safety::Reason::Thermal)
            );
        } else {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    if shared.runtime.lock().unwrap().boards[&board.id]
                        .lifecycle
                        .capacity(crate::unix_now(), true)
                        .accepts_work
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let record = store.board("warm-fixture").unwrap();
            assert!(record.owner_active);
            assert!(record.quarantine.is_none());
            assert!(record.qualification.is_none());
            assert_eq!(
                shared.runtime.lock().unwrap().boards[&board.id].native_pid,
                Some(pid)
            );
            stop.send(true).unwrap();
            assert!(task.await.unwrap().is_ok());
        }
        let written = std::fs::read_to_string(&commands).unwrap();
        assert_eq!(
            written
                .lines()
                .filter(|line| line.starts_with("TLSET"))
                .count(),
            if thermal && temperature < 55 { 2 } else { 1 }
        );
        if thermal {
            assert!(
                written
                    .lines()
                    .next()
                    .unwrap()
                    .contains("200.000000 0.700000")
            );
        }
        assert!(
            shared.runtime.lock().unwrap().boards[&board.id]
                .lifecycle
                .status()
                .quiesced
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn missing_old_ack_recovers_in_the_same_production_owner() {
        fixture(false, 40).await;
    }
    #[tokio::test]
    async fn thermal_recovery_probes_low_then_resumes_without_replacing_owner() {
        fixture(true, 40).await;
    }
    #[tokio::test]
    async fn current_hot_telemetry_is_not_promoted_to_production() {
        fixture(true, 60).await;
    }
}

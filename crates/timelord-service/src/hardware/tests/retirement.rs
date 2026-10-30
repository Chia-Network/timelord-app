//! Native retirement and unopened journal-owner rollback contracts.
use super::*;

#[tokio::test]
async fn failed_spawn_closes_only_the_unopened_journal_owner() {
    let dir = std::env::temp_dir().join(format!("tl-failed-spawn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("removed-client");
    std::fs::write(&binary, "fixture identity").unwrap();
    let board: BoardConfig = toml::from_str("id='spawn'\nname='Spawn'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.boards = vec![board.clone()];
    let path = dir.join("config.toml");
    let store = Arc::new(crate::safety::Store::open(&path).unwrap());
    let shared = Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState {
            safety: Some(store.clone()),
            ..Default::default()
        })),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    };
    let q = admission::qualification(&shared, &board, 3).unwrap();
    store.qualify("fixture", q.clone()).unwrap();
    std::fs::remove_file(binary).unwrap();
    for _ in 0..2 {
        store.begin_owner("fixture", q.point).unwrap();
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .entry(board.id.clone())
            .or_default()
            .lifecycle
            .start_native();
        assert!(launch_prepared(&shared, &board).await.is_err());
        let record = store.board("fixture").unwrap();
        assert!(!record.owner_active);
        assert_eq!(record.qualification, Some(q.clone()));
        assert!(record.quarantine.is_none());
        assert!(!store.needs_self_test("fixture", &q));
        assert!(
            shared.runtime.lock().unwrap().boards[&board.id]
                .lifecycle
                .status()
                .quiesced
        );
    }
    // The rollback is persisted, so restarting the service cannot mistake the
    // unsuccessful spawn for an owner that opened USB and failed to retire.
    drop(shared);
    drop(store);
    let reopened = crate::safety::Store::open(&path).unwrap();
    assert!(!reopened.needs_self_test("fixture", &q));
    assert!(reopened.board("fixture").unwrap().quarantine.is_none());
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn first_handshake_retirement_distinguishes_cancellation_failure_and_confirmation() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-handshake-stop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (confirmed, rejection, prior_fault) in [
        (true, None, false),
        (false, None, false),
        (true, Some("rejected"), false),
        (true, None, true),
        (true, Some("temperature_guard"), false),
        (true, Some("temperature_limit"), false),
    ] {
        let rejected = rejection.is_some();
        let thermal = matches!(rejection, Some("temperature_guard" | "temperature_limit"));
        let variant = rejection.unwrap_or("none");
        let binary = dir.join(format!("client-{confirmed}-{variant}-{prior_fault}"));
        let started = binary.with_extension("started");
        std::fs::write(
            &binary,
            format!(
                r#"#!/bin/sh
read command version id frequency voltage
printf 'started' > '{}'
{}
# Never acknowledge the first request. Only EOF may retire this native owner.
cat >/dev/null
{}
"#,
                started.display(),
                rejection
                    .map(|reason| format!(r#"printf 'TLERR 1 %s {reason}\n' "$id""#))
                    .unwrap_or_default(),
                if confirmed {
                    "printf '%s\\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2"
                } else {
                    ""
                }
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let board: BoardConfig = toml::from_str("id='handshake'\nname='Handshake'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap();
        let mut config = Config::default();
        config.hardware.client_binary = Some(binary);
        config.hardware.boards = vec![board.clone()];
        let store = Arc::new(
            crate::safety::Store::open(
                &dir.join(format!("{confirmed}-{variant}-{prior_fault}.toml")),
            )
            .unwrap(),
        );
        let shared = Arc::new(Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState {
                safety: Some(store.clone()),
                ..Default::default()
            })),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let q = admission::qualification(&shared, &board, 3).unwrap();
        store.qualify("fixture", q.clone()).unwrap();
        store.begin_owner("fixture", q.point).unwrap();
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .entry(board.id.clone())
            .or_default()
            .lifecycle
            .start_native();
        let child = launch(&shared, &board).await.unwrap();
        let (ready, _ready) = oneshot::channel();
        let (stop, rx) = watch::channel(false);
        let supervising = supervise(
            shared.clone(),
            board.clone(),
            child,
            ready,
            rx,
            INITIALIZATION.lock().await,
            None,
        );
        tokio::pin!(supervising);
        tokio::time::timeout(Duration::from_secs(5), async {
            if rejected {
                assert!(supervising.await.unwrap_err().to_string().contains("rejected request"));
                return;
            }
            tokio::select! {
                result = &mut supervising => panic!("native owner exited before cancellation: {result:?}"),
                _ = async {
                    while !started.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => {}
            }
            if prior_fault {
                shared.runtime.lock().unwrap().boards.get_mut(&board.id).unwrap().lifecycle.fault(
                    crate::board_lifecycle::Fault::Transport,
                    "Transport failed before topology cancellation".into(),
                    crate::unix_now(),
                );
            }
            stop.send(true).unwrap();
            supervising.await.unwrap();
        }).await.unwrap();
        let state = shared.runtime.lock().unwrap();
        let status = state.boards[&board.id].lifecycle.status();
        assert_eq!(status.quiesced, confirmed);
        assert_eq!(status.fault_reason.is_some(), rejected || prior_fault);
        assert_eq!(
            status.fault,
            thermal.then_some(crate::board_lifecycle::Fault::Thermal)
        );
        if thermal {
            assert_eq!(status.phase, crate::board_lifecycle::Phase::ThermalStop);
        }
        drop(state);
        let checkpoint_started = crate::unix_now();
        checkpoint_retired(&shared, &board, &store, "fixture").unwrap();
        let record = store.board("fixture").unwrap();
        assert_eq!(
            record.qualification,
            (confirmed && !rejected && !prior_fault).then_some(q.clone())
        );
        assert_eq!(
            record.quarantine,
            if thermal {
                Some(crate::safety::Reason::Thermal)
            } else {
                (!confirmed || rejected || prior_fault)
                    .then_some(crate::safety::Reason::UncleanOwner)
            }
        );
        assert_eq!(record.owner_active, !confirmed);
        if thermal {
            assert_eq!(
                record.admission_frequency_mhz,
                Some((q.point.frequency_mhz - 50.).max(200.))
            );
            assert!(record.retry_after_unix >= checkpoint_started + 300);
            assert!(store.needs_self_test("fixture", &q));
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

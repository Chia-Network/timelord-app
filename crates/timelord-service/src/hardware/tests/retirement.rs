//! Native retirement and unopened journal-owner rollback contracts.
use super::*;

#[tokio::test]
async fn shutdown_confirmation_requires_exact_fields_and_complete_lf_or_crlf_framing() {
    struct Chunked<'a> {
        bytes: &'a [u8],
        chunk: usize,
    }
    impl tokio::io::AsyncRead for Chunked<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            output: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let count = self.bytes.len().min(self.chunk).min(output.remaining());
            output.put_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            std::task::Poll::Ready(Ok(()))
        }
    }
    const MARKER: &str = "Hardware shutdown: engines_paused=1 safe_point_restored=1";
    let cases = [
        (format!("{MARKER}\n"), true),
        (format!("{MARKER}\r\n"), true),
        (format!("2026-11-01 INFO {MARKER}\r\n"), true),
        (
            format!(
                "{}\r\n{MARKER}\r\n{}",
                "x".repeat(16384),
                "tail".repeat(1024)
            ),
            true,
        ),
        (MARKER.to_string(), false),
        (format!("{MARKER}\r"), false),
        (format!("{MARKER}0\n"), false),
        (format!("{MARKER} extra\r\n"), false),
        (format!("{MARKER}\r\r\n"), false),
        ("Hardware shutdown: engines_paused=1\n".into(), false),
        (
            "Hardware shutdown: engines_paused=0 safe_point_restored=1\r\n".into(),
            false,
        ),
        (
            "Hardware shutdown: engines_paused=1 safe_point_restored=0\n".into(),
            false,
        ),
    ];
    for (record, expected) in cases {
        for chunk in [
            1,
            2,
            7,
            MARKER.len() - 1,
            MARKER.len(),
            MARKER.len() + 1,
            1024,
        ] {
            let mut source = Chunked {
                bytes: record.as_bytes(),
                chunk,
            };
            assert_eq!(
                drain_shutdown(&mut source).await.unwrap(),
                expected,
                "chunk={chunk}, record={record:?}"
            );
        }
    }
}

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
        native: if config
            .hardware
            .boards
            .iter()
            .any(|b| b.profile == ProfileMode::Automatic)
        {
            Some(Facts::load(&config).await.unwrap())
        } else {
            None
        },
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        process: crate::operational::State::fixture(),
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Arc::new(std::sync::Mutex::new({
            let mut state = crate::runtime::RuntimeState::new(false);
            state.safety = Some(store.clone());
            state
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
        let startup = startup(&shared, &board).await.unwrap();
        assert!(launch_prepared(&shared, &board, &startup).await.is_err());
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
                    // The clean cancellation fixture exercises native CRLF too.
                    let ending = if !rejected && !prior_fault { "\\r\\n" } else { "\\n" };
                    format!("printf '%s{ending}' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2")
                } else {
                    String::new()
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
        let metrics = crate::observability::Metrics::new(config.node.network.clone()).unwrap();
        let shared = Arc::new(Shared {
            native: if config
                .hardware
                .boards
                .iter()
                .any(|b| b.profile == ProfileMode::Automatic)
            {
                Some(Facts::load(&config).await.unwrap())
            } else {
                None
            },
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            process: crate::operational::State::new(metrics.clone()),
            scope: crate::runtime::RuntimeScope::Production,
            runtime: Arc::new(std::sync::Mutex::new({
                let mut state = crate::runtime::RuntimeState::new(false);
                state.safety = Some(store.clone());
                state
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
        let startup = startup(&shared, &board).await.unwrap();
        let child = launch(&shared, &board, &startup).await.unwrap();
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
            startup,
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
            metrics.counter(
                "thermal_stops_total",
                &[&crate::observability::board(&shared.config, &board.id)]
            ),
            u64::from(thermal),
            "initial thermal rejections must emit one stop; other handshakes emit none"
        );
        assert_eq!(
            state
                .events
                .iter()
                .filter(|event| event.kind == "thermal_stop")
                .count(),
            usize::from(thermal)
        );
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

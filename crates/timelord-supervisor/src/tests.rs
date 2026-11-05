use super::*;
#[tokio::test]
async fn control_accept_errors_back_off_and_keep_listener_and_worker() {
    let dir = std::env::temp_dir().join(format!("tl-accept-retry-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let helper = dir.join("worker");
    fs::write(&helper, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let worker = Worker::spawn(
        &helper,
        &dir.join("config.toml"),
        &dir.join("progress.sock"),
        1,
    )
    .unwrap();
    let pid = worker.process.pid() as i32;
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let inode = fs::symlink_metadata(&socket).unwrap().ino();
    let mut state = SupervisorState::new(Ok(Intent::default()));
    state.lifecycle = Lifecycle::Running(Box::new(worker));
    let (snapshots, current) = watch::channel(state.published());
    let (tx, _rx) = mpsc::channel(16);
    let started = Instant::now();
    let acceptor = tokio::spawn(accept_controls_with(
        AcceptBackoff::with_injected([
            std::io::Error::from_raw_os_error(libc::ECONNABORTED),
            std::io::Error::from_raw_os_error(libc::EMFILE),
        ]),
        listener,
        current,
        tx,
    ));
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    stream
        .write_all(b"{\"command\":\"status\"}\n")
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let response: Response = serde_json::from_str(&line).unwrap();
    assert_eq!(response.status.worker_pid, Some(pid as u32));
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "Errors must not busy-loop"
    );
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), inode);
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    assert!(!acceptor.is_finished());
    acceptor.abort();
    let _ = acceptor.await;
    state.retire(&snapshots).await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn interrupted_wait_retains_worker_identity_and_graceful_cleanup() {
    let dir = std::env::temp_dir().join(format!("tl-wait-interrupted-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let helper = dir.join("worker");
    fs::write(&helper, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let worker = Worker::spawn(
        &helper,
        &dir.join("config.toml"),
        &dir.join("progress.sock"),
        1,
    )
    .unwrap();
    let pid = worker.process.pid() as i32;
    worker
        .process
        .wait_errors
        .lock()
        .unwrap()
        .extend([libc::EINTR, libc::EINTR]);
    let mut state = SupervisorState::new(Ok(Intent::default()));
    state.lifecycle = Lifecycle::Running(Box::new(worker));
    let (snapshots, _) = watch::channel(state.published());
    for _ in 0..2 {
        state
            .tick(
                &helper,
                &dir.join("config.toml"),
                &dir.join("progress.sock"),
                &snapshots,
            )
            .await
            .unwrap();
        assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
        assert_eq!(state.restarts, 0);
        assert!(state.needs_power());
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    }
    let Lifecycle::Running(worker) = &state.lifecycle else {
        panic!("worker ownership lost")
    };
    worker
        .process
        .wait_errors
        .lock()
        .unwrap()
        .push_back(libc::EINVAL);
    assert!(
        worker
            .process
            .exited()
            .unwrap_err()
            .to_string()
            .contains("Observing owned process exit")
    );
    worker
        .process
        .wait_errors
        .lock()
        .unwrap()
        .push_back(libc::EINTR);
    state.retire(&snapshots).await.unwrap();
    assert_eq!(state.snapshot().worker_pid, None);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn failed_pause_retains_owner_and_power_until_confirmed_cleanup() {
    let dir = std::env::temp_dir().join(format!("tl-pause-owner-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let helper = dir.join("worker");
    fs::write(&helper, "#!/bin/sh\ncat >/dev/null\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    for resume in [false, true] {
        let mut worker =
            Worker::spawn(&helper, &paths.config, &dir.join("progress.sock"), 1).unwrap();
        let pid = worker.process.pid() as i32;
        worker.stop_failures = if resume { 2 } else { 1 };
        let mut state = SupervisorState::new(Ok(Intent::default()));
        state.lifecycle = Lifecycle::Running(Box::new(worker));
        let (snapshots, _) = watch::channel(state.published());
        let request = |command| Request {
            command,
            expected_revision: None,
        };
        assert!(
            state
                .control(&paths, request(Control::Pause), &snapshots)
                .await
                .is_err()
        );
        assert_eq!(paths.load_intent().unwrap().mode, Mode::Paused);
        assert!(state.needs_power());
        assert_eq!(state.snapshot().phase, SupervisorPhase::Stopping);
        assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
        assert!(state.snapshot().error.unwrap().contains("cleanup failed"));
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        // No replacement is admitted while the cleanup retry is pending.
        state
            .tick(
                &helper,
                &paths.config,
                &dir.join("progress.sock"),
                &snapshots,
            )
            .await
            .unwrap();
        assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
        assert_eq!(state.generation, 0);
        if resume {
            assert!(
                state
                    .control(&paths, request(Control::Resume), &snapshots)
                    .await
                    .is_err()
            );
            assert_eq!(state.intent.mode, Mode::Running);
            assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
            assert!(matches!(state.lifecycle, Lifecycle::CleanupFailed { .. }));
            state
                .control(&paths, request(Control::Pause), &snapshots)
                .await
                .unwrap();
        } else {
            let Lifecycle::CleanupFailed { next, .. } = &mut state.lifecycle else {
                panic!("cleanup ownership lost")
            };
            *next = Instant::now();
            state
                .tick(
                    &helper,
                    &paths.config,
                    &dir.join("progress.sock"),
                    &snapshots,
                )
                .await
                .unwrap();
        }
        assert!(matches!(state.lifecycle, Lifecycle::Paused));
        assert!(!state.needs_power());
        assert_eq!(state.snapshot().worker_pid, None);
        assert_eq!(state.generation, 0);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn explicit_resume_retries_paused_start_backoff_but_repeated_resume_does_not() {
    let dir = std::env::temp_dir().join(format!("tl-resume-cleanup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    let mut state = SupervisorState::new(Ok(Intent {
        mode: Mode::Paused,
        ..Default::default()
    }));
    state.lifecycle = Lifecycle::Backoff {
        next: Instant::now() + Duration::from_secs(120),
        error: "Worker start failed".into(),
    };
    let (snapshots, _) = watch::channel(state.published());
    let resume = || Request {
        command: Control::Resume,
        expected_revision: None,
    };
    state.control(&paths, resume(), &snapshots).await.unwrap();
    assert_eq!(state.intent.mode, Mode::Running);
    assert!(matches!(state.lifecycle, Lifecycle::Starting));
    let revision = state.intent.revision;
    state.lifecycle = Lifecycle::Backoff {
        next: Instant::now() + Duration::from_secs(120),
        error: "Worker crashed".into(),
    };
    state.control(&paths, resume(), &snapshots).await.unwrap();
    assert!(matches!(state.lifecycle, Lifecycle::Backoff { .. }));
    assert_eq!(state.intent.revision, revision);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn initial_status_projects_durable_pause_and_invalid_state() {
    let paused = SupervisorState::new(Ok(Intent {
        mode: Mode::Paused,
        revision: 17,
        ..Default::default()
    }));
    let status = paused.snapshot();
    assert_eq!(status.mode, Mode::Paused);
    assert_eq!(status.phase, SupervisorPhase::Paused);
    assert_eq!(status.revision, 17);
    assert_eq!(status.worker_pid, None);
    assert!(!paused.needs_power());
    let invalid = SupervisorState::new(Err(anyhow::anyhow!("corrupt fixture")));
    assert_eq!(invalid.snapshot().phase, SupervisorPhase::StateError);
    assert!(invalid.snapshot().error.is_some());
    assert!(!invalid.needs_power());
}
#[test]
fn power_follows_requested_run_through_recovery_and_actual_pause_cleanup() {
    let mut state = SupervisorState::new(Ok(Intent::default()));
    assert!(state.needs_power());
    state.lifecycle = Lifecycle::Backoff {
        next: Instant::now(),
        error: "fixture outage".into(),
    };
    assert!(state.needs_power());
    assert_eq!(state.snapshot().phase, SupervisorPhase::Recovering);
    state.intent.mode = Mode::Paused;
    state.lifecycle = Lifecycle::Stopping { pid: 123 };
    assert!(state.needs_power());
    assert_eq!(state.snapshot().worker_pid, Some(123));
    assert_eq!(state.snapshot().phase, SupervisorPhase::Stopping);
    state.lifecycle = Lifecycle::Paused;
    assert!(!state.needs_power());
    assert_eq!(state.snapshot().worker_pid, None);
}
#[test]
fn stable_worker_resets_crash_streak_but_short_crashes_keep_backoff() {
    let mut failures = 0;
    for expected in [1, 5, 30, 60, 120, 120] {
        assert_eq!(
            retry_delay(&mut failures, Some(Duration::from_secs(1))),
            expected
        );
    }
    assert_eq!(retry_delay(&mut failures, Some(Duration::from_secs(60))), 1);
    assert_eq!(retry_delay(&mut failures, None), 5);
}

use super::*;
/// Write an executable fixture and return its path. The write runs in a child
/// because Linux refuses to exec a file while any process holds it open for
/// writing: a descriptor opened here can be inherited by an unrelated test's
/// `fork` until that child execs, which makes the fixture's own exec fail with
/// ETXTBSY. The child owns the only writable descriptor, and waiting for it
/// closes that descriptor before the fixture is ever exec'd.
pub(crate) fn script_helper(dir: &Path, name: &str, body: &str) -> PathBuf {
    let helper = dir.join(name);
    let mut writer = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "fixture"])
        .arg(&helper)
        .stdin(Stdio::piped())
        .spawn()
        .expect("fixture writer");
    writer
        .stdin
        .take()
        .expect("fixture stdin")
        .write_all(body.as_bytes())
        .expect("fixture body");
    assert!(writer.wait().expect("fixture writer").success());
    helper
}

#[test]
fn script_helper_writes_the_whole_executable() {
    let dir = std::env::temp_dir().join(format!("tl-script-helper-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let helper = script_helper(&dir, "worker", "#!/bin/sh\nexit 3\n");
    assert_eq!(fs::read_to_string(&helper).unwrap(), "#!/bin/sh\nexit 3\n");
    assert_eq!(fs::symlink_metadata(&helper).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        std::process::Command::new(&helper).status().unwrap().code(),
        Some(3)
    );
    fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn completed_validator_is_reaped_before_reporting_success_or_failure() {
    let dir = std::env::temp_dir().join(format!("tl-validator-completed-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    for code in [0, 7] {
        let pid_file = dir.join(format!("pid-{code}"));
        let helper = script_helper(
            &dir,
            &format!("validator-{code}"),
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexit {code}\n",
                pid_file.display()
            ),
        );
        let result = validate_before_reload(&helper, &dir.join("config.toml")).await;
        if code == 0 {
            result.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("validation failed")
            );
        }
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
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
async fn timed_out_validator_is_group_killed_and_reaped_before_reload_returns() {
    let dir = std::env::temp_dir().join(format!("tl-validator-timeout-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("pid");
    let helper = script_helper(
        &dir,
        "validator",
        &format!(
            "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 30\n",
            pid_file.display()
        ),
    );
    let error = validate_before_reload(&helper, &dir.join("config.toml"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("validation timed out"));
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn control_accept_errors_back_off_and_keep_listener_and_worker() {
    let dir = std::env::temp_dir().join(format!("tl-accept-retry-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let helper = script_helper(&dir, "worker", "#!/bin/sh\ncat >/dev/null\n");
    let log = timelord_supervisor::logging::Writer::stderr();
    let worker = Worker::spawn(
        &helper,
        &dir.join("config.toml"),
        &dir.join("progress.sock"),
        1,
        &log,
    )
    .unwrap();
    let pid = worker.process.pid() as i32;
    let socket = dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let inode = fs::symlink_metadata(&socket).unwrap().ino();
    let mut state = SupervisorState::new(Ok(Intent::default()), log.clone());
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
        log,
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
    state.retire_now(&snapshots).await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn interrupted_wait_retains_worker_identity_and_graceful_cleanup() {
    let dir = std::env::temp_dir().join(format!("tl-wait-interrupted-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let helper = script_helper(&dir, "worker", "#!/bin/sh\ncat >/dev/null\n");
    let log = timelord_supervisor::logging::Writer::stderr();
    let worker = Worker::spawn(
        &helper,
        &dir.join("config.toml"),
        &dir.join("progress.sock"),
        1,
        &log,
    )
    .unwrap();
    let pid = worker.process.pid() as i32;
    worker
        .process
        .wait_errors
        .lock()
        .unwrap()
        .extend([libc::EINTR, libc::EINTR]);
    let mut state = SupervisorState::new(Ok(Intent::default()), log.clone());
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
    state.retire_now(&snapshots).await.unwrap();
    assert_eq!(state.snapshot().worker_pid, None);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn failed_pause_retains_owner_and_power_until_confirmed_cleanup() {
    let dir = std::env::temp_dir().join(format!("tl-pause-owner-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let helper = script_helper(&dir, "worker", "#!/bin/sh\ncat >/dev/null\n");
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    for resume in [false, true] {
        let log = timelord_supervisor::logging::Writer::stderr();
        let mut worker =
            Worker::spawn(&helper, &paths.config, &dir.join("progress.sock"), 1, &log).unwrap();
        let pid = worker.process.pid() as i32;
        worker.stop_failures = if resume { 2 } else { 1 };
        let mut state = SupervisorState::new(Ok(Intent::default()), log);
        state.lifecycle = Lifecycle::Running(Box::new(worker));
        let (snapshots, _) = watch::channel(state.published());
        let request = |command| Request {
            command,
            expected_revision: None,
        };
        // Pause is acknowledged once durable; retirement continues in the
        // background and its failure surfaces through status.
        state
            .control(&paths, &helper, request(Control::Pause), &snapshots)
            .await
            .unwrap();
        assert_eq!(paths.load_intent().unwrap().mode, Mode::Paused);
        assert!(matches!(state.lifecycle, Lifecycle::Stopping(_)));
        state.poll_retirement(&snapshots, true).await;
        assert!(state.needs_power());
        assert_eq!(state.snapshot().phase, SupervisorPhase::Stopping);
        assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
        assert!(state.snapshot().error.unwrap().contains("cleanup failed"));
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        assert!(matches!(state.cleanup_cause(), Cause::Requested));
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
            state
                .control(&paths, &helper, request(Control::Resume), &snapshots)
                .await
                .unwrap();
            state.poll_retirement(&snapshots, true).await;
            assert_eq!(state.intent.mode, Mode::Running);
            assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
            assert!(matches!(state.lifecycle, Lifecycle::CleanupFailed { .. }));
            assert_eq!(state.generation, 0);
            state
                .control(&paths, &helper, request(Control::Pause), &snapshots)
                .await
                .unwrap();
            state.poll_retirement(&snapshots, true).await;
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
            assert!(matches!(state.lifecycle, Lifecycle::Stopping(_)));
            state.poll_retirement(&snapshots, true).await;
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
    let mut state = SupervisorState::new(
        Ok(Intent {
            mode: Mode::Paused,
            ..Default::default()
        }),
        timelord_supervisor::logging::Writer::stderr(),
    );
    state.lifecycle = Lifecycle::Backoff {
        next: Instant::now() + Duration::from_secs(120),
        error: "Worker start failed".into(),
    };
    let (snapshots, _) = watch::channel(state.published());
    let resume = || Request {
        command: Control::Resume,
        expected_revision: None,
    };
    state
        .control(&paths, Path::new("/unused/worker"), resume(), &snapshots)
        .await
        .unwrap();
    assert_eq!(state.intent.mode, Mode::Running);
    assert!(matches!(state.lifecycle, Lifecycle::Starting));
    let revision = state.intent.revision;
    state.lifecycle = Lifecycle::Backoff {
        next: Instant::now() + Duration::from_secs(120),
        error: "Worker crashed".into(),
    };
    state
        .control(&paths, Path::new("/unused/worker"), resume(), &snapshots)
        .await
        .unwrap();
    assert!(matches!(state.lifecycle, Lifecycle::Backoff { .. }));
    assert_eq!(state.intent.revision, revision);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn initial_status_projects_durable_pause_and_invalid_state() {
    let paused = SupervisorState::new(
        Ok(Intent {
            mode: Mode::Paused,
            revision: 17,
            ..Default::default()
        }),
        timelord_supervisor::logging::Writer::stderr(),
    );
    let status = paused.snapshot();
    assert_eq!(status.mode, Mode::Paused);
    assert_eq!(status.phase, SupervisorPhase::Paused);
    assert_eq!(status.revision, 17);
    assert_eq!(status.worker_pid, None);
    assert!(!paused.needs_power());
    let invalid = SupervisorState::new(
        Err(anyhow::anyhow!("corrupt fixture")),
        timelord_supervisor::logging::Writer::stderr(),
    );
    assert_eq!(invalid.snapshot().phase, SupervisorPhase::StateError);
    assert!(invalid.snapshot().error.is_some());
    assert!(!invalid.needs_power());
}
#[tokio::test]
async fn power_follows_requested_run_through_recovery_and_actual_pause_cleanup() {
    let mut state = SupervisorState::new(
        Ok(Intent::default()),
        timelord_supervisor::logging::Writer::stderr(),
    );
    assert!(state.needs_power());
    state.lifecycle = Lifecycle::Backoff {
        next: Instant::now(),
        error: "fixture outage".into(),
    };
    assert!(state.needs_power());
    assert_eq!(state.snapshot().phase, SupervisorPhase::Recovering);
    state.intent.mode = Mode::Paused;
    let task = tokio::spawn(std::future::pending());
    task.abort();
    state.lifecycle = Lifecycle::Stopping(Box::new(Retiring {
        pid: 123,
        uptime: Duration::ZERO,
        cause: Cause::Requested,
        restart: false,
        task,
    }));
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

/// A worker that takes `exit_delay` seconds to honour SIGTERM. Each started
/// family records `alive.<pid>` until it exits, and flags `overlap` if it
/// starts while an earlier family is still alive. Validation passes unless
/// `<helper>.reject` exists.
fn slow_exit_helper(dir: &Path, exit_delay: &str) -> PathBuf {
    let d = dir.display();
    script_helper(
        dir,
        "worker",
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = validate ]; then [ ! -e \"$0.reject\" ]; exit $?; fi\n\
             for f in '{d}'/alive.*; do [ -e \"$f\" ] && touch '{d}/overlap'; done\n\
             touch '{d}/alive.'$$\n\
             trap \"sleep {exit_delay}; rm -f '{d}/alive.'$$; exit 0\" TERM\n\
             while :; do sleep 0.05; done\n"
        ),
    )
}
fn alive_families(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("alive."))
        })
        .count()
}
fn reaped(pid: i32) -> bool {
    let mut status = 0;
    let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}
async fn running_state(dir: &Path, helper: &Path) -> (SupervisorState, i32) {
    let log = timelord_supervisor::logging::Writer::stderr();
    let mut state = SupervisorState::new(Ok(Intent::default()), log);
    let (snapshots, _) = watch::channel(state.published());
    state
        .tick(
            helper,
            &dir.join("config.toml"),
            &dir.join("progress.sock"),
            &snapshots,
        )
        .await
        .unwrap();
    let pid = state.snapshot().worker_pid.expect("worker started") as i32;
    tokio::time::timeout(Duration::from_secs(3), async {
        while alive_families(dir) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture worker did not start");
    (state, pid)
}
fn command(command: Control) -> Request {
    Request {
        command,
        expected_revision: None,
    }
}

#[tokio::test]
async fn reload_returns_while_slow_worker_retires_and_pause_during_stopping_ends_paused() {
    let dir = std::env::temp_dir().join(format!("tl-reload-pause-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let helper = slow_exit_helper(&dir, "2");
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    let progress = dir.join("progress.sock");
    let (mut state, pid) = running_state(&dir, &helper).await;
    let (snapshots, _) = watch::channel(state.published());

    let started = Instant::now();
    state
        .control(&paths, &helper, command(Control::Reload), &snapshots)
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "Reload waited for worker retirement: {:?}",
        started.elapsed()
    );
    let stopping = state.snapshot();
    assert_eq!(stopping.phase, SupervisorPhase::Stopping);
    assert_eq!(stopping.worker_pid, Some(pid as u32));
    assert!(stopping.progress.is_none());

    // Pause during retirement is durable at once and acknowledged promptly.
    let started = Instant::now();
    state
        .control(&paths, &helper, command(Control::Pause), &snapshots)
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(paths.load_intent().unwrap().mode, Mode::Paused);
    assert_eq!(state.snapshot().mode, Mode::Paused);
    assert_eq!(state.snapshot().phase, SupervisorPhase::Stopping);
    // A further Reload coalesces into the retirement in flight.
    state
        .control(&paths, &helper, command(Control::Reload), &snapshots)
        .await
        .unwrap();
    assert_eq!(state.snapshot().worker_pid, Some(pid as u32));

    // The loop keeps ticking; nothing starts while the old family is alive.
    tokio::time::timeout(Duration::from_secs(15), async {
        while matches!(state.lifecycle, Lifecycle::Stopping(_)) {
            assert_eq!(state.generation, 1);
            state
                .tick(&helper, &paths.config, &progress, &snapshots)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("retirement did not complete");
    for _ in 0..5 {
        state
            .tick(&helper, &paths.config, &progress, &snapshots)
            .await
            .unwrap();
    }
    assert!(matches!(state.lifecycle, Lifecycle::Paused));
    assert_eq!(state.snapshot().phase, SupervisorPhase::Paused);
    assert_eq!(state.snapshot().worker_pid, None);
    assert_eq!(
        state.generation, 1,
        "Pause during stopping started a worker"
    );
    assert!(!state.needs_power());
    assert!(reaped(pid));
    assert_eq!(alive_families(&dir), 0);
    assert!(!dir.join("overlap").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn resume_and_reload_during_stopping_start_exactly_one_replacement_after_reap() {
    let dir = std::env::temp_dir().join(format!("tl-reload-resume-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let helper = slow_exit_helper(&dir, "1");
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    let progress = dir.join("progress.sock");
    let (mut state, pid) = running_state(&dir, &helper).await;
    let (snapshots, _) = watch::channel(state.published());

    state
        .control(&paths, &helper, command(Control::Pause), &snapshots)
        .await
        .unwrap();
    state
        .control(&paths, &helper, command(Control::Resume), &snapshots)
        .await
        .unwrap();
    for _ in 0..3 {
        state
            .control(&paths, &helper, command(Control::Reload), &snapshots)
            .await
            .unwrap();
    }
    assert_eq!(state.intent.mode, Mode::Running);
    assert_eq!(state.snapshot().phase, SupervisorPhase::Stopping);
    tokio::time::timeout(Duration::from_secs(15), async {
        while state.generation == 1 {
            assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
            state
                .tick(&helper, &paths.config, &progress, &snapshots)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("replacement did not start");
    // The old family was reaped before its replacement was spawned.
    assert!(reaped(pid));
    let replacement = state.snapshot();
    assert_eq!(replacement.phase, SupervisorPhase::WorkerStarted);
    assert_ne!(replacement.worker_pid, Some(pid as u32));
    for _ in 0..5 {
        state
            .tick(&helper, &paths.config, &progress, &snapshots)
            .await
            .unwrap();
    }
    assert_eq!(
        state.generation, 2,
        "coalesced reloads started extra workers"
    );
    assert_eq!(state.restarts, 0);
    tokio::time::timeout(Duration::from_secs(3), async {
        while alive_families(&dir) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!dir.join("overlap").exists());
    state.retire_now(&snapshots).await.unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn retirement_grace_depends_on_watchdog_classification() {
    use timelord_supervisor::progress::Failure;
    let unresponsive = Failure::unresponsive("heartbeat");
    let reported = Failure::reported("essential");
    let normal = timelord_supervisor::NORMAL_GRACEFUL_STOP;
    assert_eq!(
        Cause::Failed {
            watchdog: Some(unresponsive)
        }
        .grace(),
        timelord_supervisor::WATCHDOG_GRACEFUL_STOP
    );
    assert_eq!(
        Cause::Failed {
            watchdog: Some(reported)
        }
        .grace(),
        normal
    );
    assert_eq!(Cause::Requested.grace(), normal);
    assert_eq!(Cause::CleanupRetry.grace(), Duration::ZERO);
    assert_eq!(Cause::Failed { watchdog: None }.grace(), normal);
}

#[tokio::test]
async fn unresponsive_watchdog_failure_reaps_under_the_short_grace() {
    use timelord_supervisor::progress::Failure;
    let dir = std::env::temp_dir().join(format!("tl-unresponsive-grace-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    let helper = script_helper(
        &dir,
        "worker",
        &format!("#!/bin/sh\ntouch '{d}/alive.'$$\ntrap '' TERM\nwhile :; do sleep 0.1; done\n"),
    );
    let (mut state, pid) = running_state(&dir, &helper).await;
    let (snapshots, _) = watch::channel(state.published());
    let started = Instant::now();
    state.begin_retirement(Cause::Failed {
        watchdog: Some(Failure::unresponsive("fixture heartbeat expiry")),
    });
    tokio::time::timeout(
        Duration::from_secs(6),
        state.poll_retirement(&snapshots, true),
    )
    .await
    .expect("unresponsive retirement exceeded 6 s; it must not use the normal 25 s wait");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "unresponsive retirement took {:?}",
        started.elapsed()
    );
    assert!(reaped(pid));
    assert!(matches!(state.lifecycle, Lifecycle::Backoff { .. }));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cleanup_retry_skips_the_graceful_wait() {
    use timelord_supervisor::progress::Failure;
    let dir = std::env::temp_dir().join(format!("tl-cleanup-retry-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    let helper = script_helper(
        &dir,
        "worker",
        &format!("#!/bin/sh\ntouch '{d}/alive.'$$\ntrap '' TERM\nwhile :; do sleep 0.1; done\n"),
    );
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    let (mut state, pid) = running_state(&dir, &helper).await;
    let (snapshots, _) = watch::channel(state.published());
    state.lifecycle.worker_mut().unwrap().stop_fail_after_kill = 1;
    state.begin_retirement(Cause::Failed {
        watchdog: Some(Failure::unresponsive("fixture cleanup retry")),
    });
    state.poll_retirement(&snapshots, true).await;
    assert!(matches!(state.lifecycle, Lifecycle::CleanupFailed { .. }));
    assert!(
        state.lifecycle.worker().unwrap().killed,
        "a stop that reached the group SIGKILL must be marked"
    );
    assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
    let started = Instant::now();
    state
        .control(&paths, &helper, command(Control::Pause), &snapshots)
        .await
        .unwrap();
    state.poll_retirement(&snapshots, true).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "CleanupRetry waited for the graceful budget: {:?}",
        started.elapsed()
    );
    assert!(matches!(state.lifecycle, Lifecycle::Paused));
    assert!(reaped(pid));
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn worker_stop_bounds_group_kill_and_log_drain() {
    let dir = std::env::temp_dir().join(format!("tl-stop-bound-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    let helper = script_helper(
        &dir,
        "worker",
        &format!(
            "#!/bin/sh\ntrap '' TERM\n\
             /usr/bin/perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' /bin/sh -c 'echo $$ > {d}/grandchild; exec /bin/sleep 15' &\n\
             while :; do sleep 0.1; done\n"
        ),
    );
    let log = timelord_supervisor::logging::Writer::stderr();
    let mut worker = Worker::spawn(
        &helper,
        &dir.join("config.toml"),
        &dir.join("progress.sock"),
        1,
        &log,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !dir.join("grandchild").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached grandchild did not start");
    let grandchild: i32 = fs::read_to_string(dir.join("grandchild"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = Instant::now();
    let status = worker.stop(Duration::ZERO).await.unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= LOG_DRAIN,
        "Worker::stop did not wait for the aborted log drain: {elapsed:?}"
    );
    assert!(
        elapsed < LOG_DRAIN + Duration::from_secs(1),
        "Worker::stop took {elapsed:?}"
    );
    assert!(!status.success());
    unsafe {
        libc::kill(grandchild, libc::SIGKILL);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cancelled_validation_reaps_the_validator_group() {
    let dir = std::env::temp_dir().join(format!("tl-validator-cancel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("pid");
    let helper = script_helper(
        &dir,
        "validator",
        &format!(
            "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 30\n",
            pid_file.display()
        ),
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        validate_before_reload(&helper, &dir.join("config.toml")),
    )
    .await
    .expect_err("validation should be cancelled");
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn rejected_reload_retains_the_running_worker() {
    let dir = std::env::temp_dir().join(format!("tl-reload-reject-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let helper = slow_exit_helper(&dir, "0");
    let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
    let (mut state, pid) = running_state(&dir, &helper).await;
    let (snapshots, _) = watch::channel(state.published());
    fs::write(helper.with_extension("reject"), b"fixture").unwrap();
    let error = state
        .control(&paths, &helper, command(Control::Reload), &snapshots)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("validation failed"));
    assert!(matches!(state.lifecycle, Lifecycle::Running(_)));
    assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
    assert_eq!(state.generation, 1);
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    state.retire_now(&snapshots).await.unwrap();
    assert!(reaped(pid));
    fs::remove_dir_all(dir).unwrap();
}

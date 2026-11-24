use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
use timelord_supervisor::SupervisorPhase;
use timelord_supervisor::{Control, Intent, Mode, Paths, Status, request};
use tokio::{process::Command, time::sleep};

mod common;
use common::Fixture;

/// Wait until the worker shell recorded `count` starts. The supervisor
/// publishes WorkerStarted when it spawns the process, before the shell runs,
/// so a status check alone can precede the pid line it is about to write.
async fn worker_starts(fixture: &Fixture, count: usize) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(pids) = fs::read_to_string(fixture.0.join("pids"))
                && pids.lines().filter(|line| !line.trim().is_empty()).count() >= count
            {
                return pids;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker starts were not recorded")
}
async fn ensure_session(fixture: &Fixture, helper: &Path) -> Status {
    let output = Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"))
        .arg("ensure-started")
        .arg("--config")
        .arg(fixture.config())
        .arg("--worker")
        .arg(helper)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn stop_session(fixture: &Fixture, status: &Status) {
    unsafe {
        libc::kill(status.supervisor_pid as i32, libc::SIGTERM);
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        while request(&fixture.config(), Control::Status, None)
            .await
            .is_ok()
        {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_start_survives_cli_exit_reuses_owner_and_preserves_pause() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let first = ensure_session(&fixture, &helper).await;
    assert_eq!(first.mode, Mode::Running);
    fixture.status(|s| s.worker_pid.is_some()).await;
    let second = ensure_session(&fixture, &helper).await;
    assert_eq!(first.supervisor_pid, second.supervisor_pid);
    let paused = request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    assert_eq!(ensure_session(&fixture, &helper).await.mode, Mode::Paused);
    stop_session(&fixture, &paused).await;
    let restarted = ensure_session(&fixture, &helper).await;
    assert_eq!(restarted.mode, Mode::Paused);
    assert!(restarted.worker_pid.is_none());
    stop_session(&fixture, &restarted).await;
}

#[tokio::test]
async fn concurrent_session_starters_share_one_supervisor() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let (a, b) = tokio::join!(
        ensure_session(&fixture, &helper),
        ensure_session(&fixture, &helper)
    );
    assert_eq!(a.supervisor_pid, b.supervisor_pid);
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    stop_session(&fixture, &a).await;
}

#[tokio::test]
async fn session_start_refuses_an_existing_foreground_worker_listener() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"))
        .arg("ensure-started")
        .arg("--config")
        .arg(fixture.config())
        .arg("--worker")
        .arg(&helper)
        .arg("--status-listen")
        .arg(listener.local_addr().unwrap().to_string())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("existing worker"));
    assert!(!fixture.0.join("pids").exists());
}

#[tokio::test]
async fn explicit_pause_survives_supervisor_restart_and_resume_uses_one_owner() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let first = fixture.supervisor(&helper);
    let running = fixture.status(|s| s.worker_pid.is_some()).await;
    assert_eq!(running.mode, Mode::Running);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("pids").exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // A second supervisor cannot steal the socket or start another worker.
    let mut duplicate = fixture.supervisor(&helper);
    assert!(!duplicate.wait().await.unwrap().success());
    let paused = request(&fixture.config(), Control::Pause, Some(running.revision))
        .await
        .unwrap();
    assert_eq!(paused.mode, Mode::Paused);
    // Pause is acknowledged once durable; follow status until cleanup completes.
    let paused = fixture.status(|s| s.phase == SupervisorPhase::Paused).await;
    assert!(paused.worker_pid.is_none());
    let old = running.worker_pid.unwrap();
    assert_eq!(unsafe { libc::kill(old as i32, 0) }, -1);
    fixture.terminate(first).await;
    let second = fixture.supervisor(&helper);
    let restored = fixture.status(|s| s.mode == Mode::Paused).await;
    assert_eq!(restored.phase, SupervisorPhase::Paused);
    assert!(restored.error.is_none());
    assert_eq!(restored.revision, paused.revision);
    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fs::read_to_string(fixture.0.join("pids"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    // Stale control commands must not overwrite an operator's newer decision.
    assert!(
        request(&fixture.config(), Control::Resume, Some(running.revision))
            .await
            .is_err()
    );
    request(&fixture.config(), Control::Resume, Some(paused.revision))
        .await
        .unwrap();
    let resumed = fixture.status(|s| s.worker_pid.is_some()).await;
    assert_ne!(resumed.worker_pid, Some(old));
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(second).await;
}

#[tokio::test]
async fn crash_is_replaced_without_gui_and_pause_cancels_backoff() {
    let fixture = Fixture::new();
    let marker = fixture.0.join("first");
    let helper = fixture.helper(&format!(
        "if [ ! -e '{}' ]; then touch '{}'; exit 17; fi\nexec /bin/sleep 60",
        marker.display(),
        marker.display()
    ));
    let child = fixture.supervisor(&helper);
    let replaced = fixture
        .status(|s| s.worker_generation >= 2 && s.worker_pid.is_some())
        .await;
    assert_eq!(replaced.restarts, 1);
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(child).await;
    // A permanently crashing worker stays recoverable rather than exhausting
    // retries; an explicit pause suppresses the pending next attempt.
    let helper = fixture.helper("exit 19");
    let paths = Paths::for_config(&fixture.config()).unwrap();
    paths.save_intent(&Intent::default()).unwrap();
    let child = fixture.supervisor(&helper);
    fixture
        .status(|s| s.restarts >= 1 && s.worker_pid.is_none())
        .await;
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    let starts = fs::read_to_string(fixture.0.join("pids"))
        .unwrap()
        .lines()
        .count();
    sleep(Duration::from_millis(1300)).await;
    assert_eq!(
        starts,
        fs::read_to_string(fixture.0.join("pids"))
            .unwrap()
            .lines()
            .count()
    );
    fixture.terminate(child).await;
}

#[tokio::test]
async fn corrupt_intent_never_silently_resumes_and_failed_save_preserves_pause() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let paths = Paths::for_config(&fixture.config()).unwrap();
    paths
        .save_intent(&Intent {
            mode: Mode::Paused,
            ..Default::default()
        })
        .unwrap();
    fs::write(&paths.intent, b"corrupt").unwrap();
    let child = fixture.supervisor(&helper);
    let failed = fixture
        .status(|s| s.phase == SupervisorPhase::StateError)
        .await;
    assert_eq!(failed.mode, Mode::Paused);
    assert!(failed.worker_pid.is_none());
    assert_eq!(fs::read(&paths.intent).unwrap(), b"corrupt");
    // A human's explicit resume can replace corrupt state; passive startup cannot.
    request(&fixture.config(), Control::Resume, None)
        .await
        .unwrap();
    fixture.status(|s| s.worker_pid.is_some()).await;
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    let saved = fs::read(&paths.intent).unwrap();
    fs::write(
        timelord_supervisor::atomic::pending(&paths.intent).unwrap(),
        b"occupied",
    )
    .unwrap();
    assert!(paths.save_intent(&Intent::default()).is_err());
    assert_eq!(fs::read(&paths.intent).unwrap(), saved);
    fixture.terminate(child).await;
}

#[tokio::test]
async fn requested_run_survives_shutdown_and_worker_family_is_reaped_before_replacement() {
    let fixture = Fixture::new();
    let helper = fixture.helper(&format!(
        "/bin/sleep 60 &\necho $! > '{}'\nwait",
        fixture.0.join("descendant").display()
    ));
    let child = fixture.supervisor(&helper);
    fixture.status(|s| s.worker_pid.is_some()).await;
    let descendant: i32 = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = fs::read_to_string(fixture.0.join("descendant"))
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                break pid;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    request(&fixture.config(), Control::Reload, None)
        .await
        .unwrap();
    // The old group was signalled before its leader's PID was reaped. A
    // descendant may briefly remain a zombie until launchd/init reaps it, but
    // it must have received SIGKILL and cannot retain a USB owner indefinitely.
    fixture
        .status(|s| s.worker_generation >= 2 && s.worker_pid.is_some())
        .await;
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{descendant}/stat")) {
        assert!(stat.split_whitespace().nth(2) == Some("Z"));
    }
    #[cfg(target_os = "macos")]
    {
        let result = std::process::Command::new("ps")
            .args(["-p", &descendant.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        assert!(
            !result.status.success()
                || String::from_utf8_lossy(&result.stdout)
                    .trim()
                    .starts_with('Z')
        );
    }
    fixture.terminate(child).await;
    let next = fixture.supervisor(&helper);
    assert_eq!(
        fixture.status(|s| s.worker_pid.is_some()).await.mode,
        Mode::Running
    );
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(next).await;
}

#[test]
fn intent_and_owner_paths_reject_links_and_insecure_files() {
    use std::os::unix::fs::symlink;
    use timelord_supervisor::InstanceLock;
    let fixture = Fixture::new();
    let paths = Paths::for_config(&fixture.config()).unwrap();
    symlink(fixture.0.join("missing"), &paths.intent).unwrap();
    assert!(
        paths.load_intent().is_err(),
        "a broken symlink is not a fresh installation"
    );
    fs::remove_file(&paths.intent).unwrap();
    paths
        .save_intent(&Intent {
            mode: Mode::Paused,
            ..Default::default()
        })
        .unwrap();
    fs::set_permissions(&paths.intent, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(paths.load_intent().is_err());
    symlink(&paths.intent, &paths.lock).unwrap();
    assert!(InstanceLock::acquire(&paths.lock).is_err());
    fs::remove_file(&paths.lock).unwrap();
    let owner = InstanceLock::acquire(&paths.lock).unwrap();
    assert!(InstanceLock::acquire(&paths.lock).is_err());
    drop(owner);
    assert!(InstanceLock::acquire(&paths.lock).is_ok());
}

#[test]
fn launchd_plan_is_boot_scoped_non_root_and_escapes_paths() {
    let xml = timelord_supervisor::launchd_plist(
        Path::new("/Applications/My & Timelord.app"),
        Path::new("/Users/operator/A 'config'.toml"),
        "operator",
    )
    .unwrap();
    assert!(xml.contains("<key>UserName</key><string>operator</string>"));
    assert!(xml.contains("<key>KeepAlive</key><true/>"));
    assert!(xml.contains("My &amp; Timelord.app"));
    assert!(xml.contains("A &apos;config&apos;.toml"));
    assert!(xml.contains("<key>ProcessType</key><string>Interactive</string>"));
    assert!(
        timelord_supervisor::launchd_plist(
            Path::new("/Applications/Timelord.app"),
            Path::new("/config"),
            "root"
        )
        .is_err()
    );
    assert!(
        timelord_supervisor::launchd_plist(
            Path::new("relative.app"),
            Path::new("/config"),
            "operator"
        )
        .is_err()
    );
    assert!(
        timelord_supervisor::launchd_plist(Path::new("/app"), Path::new("/config"), "bad<key>")
            .is_err()
    );
}

#[test]
fn launchd_cli_accepts_and_preserves_a_custom_network_id() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"))
        .args([
            "launchd-plist",
            "--app",
            "/Applications/Timelord.app",
            "--config",
            "/tmp/fixture-config.toml",
            "--user",
            "operator",
            "--monitor-listen",
            "127.0.0.1:9259",
            "--network",
            "ephemeral-fixture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("<string>--network</string><string>ephemeral-fixture</string>")
    );
}

#[test]
fn parent_guard_fixture() {
    let Ok(marker) = std::env::var("TIMELORD_PARENT_FIXTURE_MARKER") else {
        return;
    };
    // Deliberately ignore graceful shutdown to exercise the independent hard
    // deadline, as if the worker's async/blocking computation pool were wedged.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    timelord_supervisor::watch_parent().unwrap();
    fs::write(marker, b"ready").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[tokio::test]
async fn abrupt_supervisor_death_cannot_leave_a_wedged_worker_running() {
    let fixture = Fixture::new();
    let marker = fixture.0.join("guard-ready");
    let test_exe = std::env::current_exe().unwrap();
    let helper = fixture.helper(&format!(
        "TIMELORD_PARENT_FIXTURE_MARKER='{}' exec '{}' --exact parent_guard_fixture --nocapture",
        marker.display(),
        test_exe.display()
    ));
    let mut child = fixture.supervisor(&helper);
    let status = fixture.status(|s| s.worker_pid.is_some()).await;
    tokio::time::timeout(Duration::from_secs(4), async {
        while !marker.exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    unsafe {
        libc::kill(child.id().unwrap() as i32, libc::SIGKILL);
    }
    child.wait().await.unwrap();
    let worker = status.worker_pid.unwrap();
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            if unsafe { libc::kill(worker as i32, 0) } == -1 {
                break;
            }
            let result = std::process::Command::new("ps")
                .args(["-p", &worker.to_string(), "-o", "stat="])
                .output()
                .unwrap();
            if !result.status.success()
                || String::from_utf8_lossy(&result.stdout)
                    .trim()
                    .starts_with('Z')
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("orphan worker exceeded its independent shutdown deadline");
}

#[test]
fn concurrent_first_status_and_start_create_private_state_atomically() {
    let fixture = Fixture::new();
    let config = fixture.config();
    std::thread::scope(|scope| {
        let mut threads = vec![];
        for _ in 0..32 {
            let config = &config;
            threads.push(scope.spawn(move || {
                let paths = Paths::for_config(config).unwrap();
                assert_eq!(
                    fs::metadata(paths.directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
    });
}

async fn monitor_get(port: u16, path: &str, method: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}
#[tokio::test]
async fn deployment_monitor_survives_pause_and_never_confuses_worker_presence_with_readiness() {
    let fixture = Fixture::new();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let helper = fixture.helper("exec /bin/sleep 60");
    let supervisor = fixture.supervisor_with_monitor(&helper, Some(port));
    fixture.status(|s| s.worker_pid.is_some()).await;
    assert!(
        monitor_get(port, "/healthz", "GET")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        monitor_get(port, "/readyz", "GET")
            .await
            .starts_with("HTTP/1.1 503")
    );
    let body = monitor_get(port, "/metrics", "GET").await;
    assert!(body.contains("chia_timelord_supervisor_worker_up{network=\"testnet11\"} 1"));
    assert!(body.contains("chia_timelord_supervisor_worker_ready{network=\"testnet11\"} 0"));
    if let Some(path) = std::env::var_os("TIMELORD_SUPERVISOR_TEST_EXPOSITION") {
        fs::write(path, body.split_once("\r\n\r\n").unwrap().1).unwrap();
    }
    let running = request(&fixture.config(), Control::Status, None)
        .await
        .unwrap();
    unsafe {
        libc::kill(running.worker_pid.unwrap() as i32, libc::SIGKILL);
    }
    fixture
        .status(|s| s.phase == timelord_supervisor::SupervisorPhase::Recovering)
        .await;
    let recovering = monitor_get(port, "/metrics", "GET").await;
    assert!(
        recovering.contains("chia_timelord_supervisor_desired_running{network=\"testnet11\"} 1")
    );
    assert!(recovering.contains("chia_timelord_supervisor_worker_up{network=\"testnet11\"} 0"));
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    assert!(
        monitor_get(port, "/healthz", "GET")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        monitor_get(port, "/readyz", "GET")
            .await
            .starts_with("HTTP/1.1 503")
    );
    assert!(
        monitor_get(port, "/metrics", "GET")
            .await
            .contains("chia_timelord_supervisor_desired_running{network=\"testnet11\"} 0")
    );
    assert!(
        monitor_get(port, "/resume", "POST")
            .await
            .starts_with("HTTP/1.1 404")
    );
    assert!(
        monitor_get(port, "/metrics", "POST")
            .await
            .starts_with("HTTP/1.1 405")
    );
    assert_eq!(
        request(&fixture.config(), Control::Status, None)
            .await
            .unwrap()
            .mode,
        Mode::Paused
    );
    fixture.terminate(supervisor).await;
}

#[tokio::test]
async fn rejected_reload_keeps_the_useful_worker_and_desired_state_unchanged() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let parent = fixture.supervisor(&helper);
    let before = fixture.status(|s| s.worker_pid.is_some()).await;
    fs::write(
        helper.with_extension("reject"),
        "fixture invalid configuration",
    )
    .unwrap();
    assert!(
        request(&fixture.config(), Control::Reload, None)
            .await
            .is_err()
    );
    let after = fixture.status(|s| s.worker_pid == before.worker_pid).await;
    assert_eq!(after.worker_generation, before.worker_generation);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.mode, Mode::Running);
    fs::remove_file(helper.with_extension("reject")).unwrap();
    request(&fixture.config(), Control::Reload, None)
        .await
        .unwrap();
    fixture
        .status(|s| s.worker_generation > before.worker_generation)
        .await;
    fixture.terminate(parent).await;
}

#[test]
fn check_owner_rejects_supervisor_or_worker_ownership() {
    let fixture = Fixture::new();
    let paths = Paths::for_config(&fixture.config()).unwrap();
    for path in [&paths.lock, &paths.worker_lock] {
        let held = timelord_supervisor::InstanceLock::acquire(path).unwrap();
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"))
            .args(["check-owner", "--config"])
            .arg(fixture.config())
            .output()
            .unwrap();
        assert!(!result.status.success());
        drop(held);
    }
    assert!(
        std::process::Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"))
            .args(["check-owner", "--config"])
            .arg(fixture.config())
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn installer_pid_extraction_matches_live_status_cli() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exec /bin/sleep 60");
    let owner = fixture.supervisor(&helper);
    let running = fixture.status(|s| s.worker_pid.is_some()).await;
    let output = Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"))
        .arg("status")
        .arg("--config")
        .arg(fixture.config())
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let script = include_str!("../../../scripts/install-service.sh");
    let reported = script
        .lines()
        .find(|line| line.trim_start().starts_with("REPORTED="))
        .unwrap();
    let key = reported
        .split("plutil -extract ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let mut parser = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut parser.stdin.take().unwrap(), &output.stdout).unwrap();
    let parsed = parser.wait_with_output().unwrap();
    assert!(parsed.status.success());
    assert_eq!(
        String::from_utf8(parsed.stdout).unwrap().trim(),
        running.supervisor_pid.to_string()
    );
    assert_eq!(Some(running.supervisor_pid), owner.id());
    fixture.terminate(owner).await;
}

#[tokio::test]
async fn pause_acknowledges_durable_intent_while_graceful_cleanup_continues() {
    let fixture = Fixture::new();
    let ready = fixture.0.join("ready");
    let helper = fixture.helper(&format!(
        "trap 'sleep 2; exit 0' TERM\ntouch '{}'\nwhile :; do sleep 1; done",
        ready.display()
    ));
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let supervisor = fixture.supervisor_with_monitor(&helper, Some(port));
    let running = fixture.status(|s| s.worker_pid.is_some()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !ready.exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let report = timelord_supervisor::progress::Snapshot {
        worker_pid: running.worker_pid.unwrap(),
        generation: running.worker_generation,
        sequence: 1,
        ready: true,
        failed: false,
        configuration_fallback: false,
        configuration_checkpointed: false,

        work: [None; timelord_supervisor::progress::SLOTS],
    };
    let socket = tokio::net::UnixDatagram::unbound().unwrap();
    socket
        .send_to(
            &serde_json::to_vec(&report).unwrap(),
            Paths::for_config(&fixture.config())
                .unwrap()
                .directory
                .join("progress.sock"),
        )
        .await
        .unwrap();
    fixture
        .status(|s| s.progress.as_ref().is_some_and(|p| p.ready))
        .await;
    assert!(
        monitor_get(port, "/readyz", "GET")
            .await
            .starts_with("HTTP/1.1 200")
    );
    // The worker takes 2s to honour SIGTERM; Pause must not wait for it.
    let acknowledged = tokio::time::timeout(
        Duration::from_secs(1),
        request(&fixture.config(), Control::Pause, None),
    )
    .await
    .expect("Pause acknowledgment waited for worker cleanup")
    .unwrap();
    assert_eq!(acknowledged.mode, Mode::Paused);
    assert_eq!(acknowledged.phase, SupervisorPhase::Stopping);
    assert_eq!(acknowledged.worker_pid, running.worker_pid);
    let stopping = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.status(|s| s.phase == SupervisorPhase::Stopping),
    )
    .await
    .expect("fresh status reader blocked behind worker cleanup");
    assert_eq!(stopping.mode, Mode::Paused);
    assert_eq!(stopping.revision, running.revision + 1);
    assert_eq!(stopping.worker_pid, running.worker_pid);
    assert!(stopping.progress.is_none());
    assert!(
        monitor_get(port, "/readyz", "GET")
            .await
            .starts_with("HTTP/1.1 503")
    );
    assert!(
        monitor_get(port, "/metrics", "GET")
            .await
            .contains("chia_timelord_supervisor_worker_ready{network=\"testnet11\"} 0")
    );
    assert_eq!(
        Paths::for_config(&fixture.config())
            .unwrap()
            .load_intent()
            .unwrap()
            .mode,
        Mode::Paused
    );
    let paused = fixture.status(|s| s.phase == SupervisorPhase::Paused).await;
    assert!(paused.worker_pid.is_none());
    assert_eq!(paused.worker_generation, running.worker_generation);
    assert_eq!(
        unsafe { libc::kill(running.worker_pid.unwrap() as i32, 0) },
        -1
    );
    fixture.terminate(supervisor).await;
}

/// Each started family records `alive.<pid>` until it exits after a slow
/// SIGTERM drain, and flags `overlap` if it starts while another is alive.
fn slow_exit_body(fixture: &Fixture, drain_seconds: u32) -> String {
    let d = fixture.0.display();
    format!(
        "for f in '{d}'/alive.*; do [ -e \"$f\" ] && touch '{d}/overlap'; done\n\
         touch '{d}/alive.'$$\n\
         trap \"sleep {drain_seconds}; rm -f '{d}/alive.'$$; exit 0\" TERM\n\
         while :; do sleep 0.05; done"
    )
}

#[tokio::test]
async fn reload_returns_before_a_slow_worker_retires_and_families_never_overlap() {
    let fixture = Fixture::new();
    let helper = fixture.helper(&slow_exit_body(&fixture, 4));
    let supervisor = fixture.supervisor(&helper);
    let running = fixture
        .status(|s| s.phase == SupervisorPhase::WorkerStarted)
        .await;
    worker_starts(&fixture, 1).await;
    let started = std::time::Instant::now();
    let reloading = request(&fixture.config(), Control::Reload, Some(running.revision))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "Reload waited for worker retirement: {:?}",
        started.elapsed()
    );
    assert_eq!(reloading.phase, SupervisorPhase::Stopping);
    assert_eq!(reloading.worker_pid, running.worker_pid);
    assert_eq!(reloading.worker_generation, running.worker_generation);
    // Repeated reloads coalesce into the retirement in flight, and the
    // control loop answers mutations while the old family drains.
    let started = std::time::Instant::now();
    let again = request(&fixture.config(), Control::Reload, None)
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(again.worker_pid, running.worker_pid);
    let replaced = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status = request(&fixture.config(), Control::Status, None)
                .await
                .unwrap();
            if status.worker_generation == running.worker_generation {
                // Either the old family is still owned, or it was reaped and
                // the replacement has not been spawned yet.
                match status.phase {
                    SupervisorPhase::Stopping => {
                        assert_eq!(status.worker_pid, running.worker_pid)
                    }
                    SupervisorPhase::Starting => assert_eq!(status.worker_pid, None),
                    phase => panic!("unexpected phase while reloading: {phase:?}"),
                }
            } else if status.phase == SupervisorPhase::WorkerStarted {
                break status;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("replacement did not start after retirement");
    assert_eq!(replaced.worker_generation, running.worker_generation + 1);
    assert_ne!(replaced.worker_pid, running.worker_pid);
    assert_eq!(
        unsafe { libc::kill(running.worker_pid.unwrap() as i32, 0) },
        -1
    );
    sleep(Duration::from_millis(300)).await;
    let settled = request(&fixture.config(), Control::Status, None)
        .await
        .unwrap();
    assert_eq!(settled.worker_generation, replaced.worker_generation);
    assert_eq!(worker_starts(&fixture, 2).await.lines().count(), 2);
    assert!(!fixture.0.join("overlap").exists());
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.status(|s| s.phase == SupervisorPhase::Paused).await;
    fixture.terminate(supervisor).await;
}

#[tokio::test]
async fn pause_during_reload_retirement_ends_paused_without_a_new_worker() {
    let fixture = Fixture::new();
    let helper = fixture.helper(&slow_exit_body(&fixture, 2));
    let supervisor = fixture.supervisor(&helper);
    let running = fixture
        .status(|s| s.phase == SupervisorPhase::WorkerStarted)
        .await;
    worker_starts(&fixture, 1).await;
    let reloading = request(&fixture.config(), Control::Reload, None)
        .await
        .unwrap();
    assert_eq!(reloading.phase, SupervisorPhase::Stopping);
    let started = std::time::Instant::now();
    let paused = request(&fixture.config(), Control::Pause, Some(reloading.revision))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(paused.mode, Mode::Paused);
    assert_eq!(paused.phase, SupervisorPhase::Stopping);
    assert_eq!(
        Paths::for_config(&fixture.config())
            .unwrap()
            .load_intent()
            .unwrap()
            .mode,
        Mode::Paused
    );
    let settled = fixture.status(|s| s.phase == SupervisorPhase::Paused).await;
    assert!(settled.worker_pid.is_none());
    sleep(Duration::from_millis(500)).await;
    let later = request(&fixture.config(), Control::Status, None)
        .await
        .unwrap();
    assert_eq!(later.phase, SupervisorPhase::Paused);
    assert_eq!(later.worker_generation, running.worker_generation);
    assert_eq!(worker_starts(&fixture, 1).await.lines().count(), 1);
    assert!(!fixture.0.join("overlap").exists());
    fixture.terminate(supervisor).await;
}

#[tokio::test]
async fn idempotent_resume_during_backoff_does_not_rewrite_intent() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exit 17");
    let supervisor = fixture.supervisor(&helper);
    let failed = fixture
        .status(|s| s.phase == SupervisorPhase::Recovering)
        .await;
    let resumed = request(&fixture.config(), Control::Resume, Some(failed.revision))
        .await
        .unwrap();
    assert_eq!(resumed.revision, failed.revision);
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(supervisor).await;
}

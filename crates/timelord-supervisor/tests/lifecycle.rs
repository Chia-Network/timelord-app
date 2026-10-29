use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use timelord_supervisor::{Control, Intent, Mode, Paths, Status, request};
use tokio::{
    process::{Child, Command},
    time::sleep,
};

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = Path::new("/tmp").join(format!(
            "tl-sup-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }
    fn helper(&self, body: &str) -> PathBuf {
        let path = self.0.join("worker");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho $$ >> '{}'\n{body}\n",
                self.0.join("pids").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn supervisor(&self, helper: &Path) -> Child {
        self.supervisor_with_monitor(helper, None)
    }
    fn supervisor_with_monitor(&self, helper: &Path, monitor: Option<u16>) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"));
        if let Some(port) = monitor {
            command.args([
                "run",
                "--monitor-listen",
                &format!("127.0.0.1:{port}"),
                "--network",
                "testnet11",
            ]);
        } else {
            command.arg("run");
        }
        command
            .arg("--config")
            .arg(self.config())
            .arg("--worker")
            .arg(helper)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }
    async fn status(&self, predicate: impl Fn(&Status) -> bool) -> Status {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(status) = request(&self.config(), Control::Status, None).await
                    && predicate(&status)
                {
                    break status;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("supervisor did not reach requested fixture state")
    }
    async fn terminate(&self, mut child: Child) {
        unsafe {
            libc::kill(child.id().unwrap() as i32, libc::SIGTERM);
        }
        let exit = tokio::time::timeout(Duration::from_secs(4), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(exit.success());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(pids) = fs::read_to_string(self.0.join("pids")) {
            for pid in pids
                .lines()
                .filter_map(|p| p.parse::<i32>().ok())
                .filter(|p| *p > 1)
            {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                };
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
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
    assert!(paused.worker_pid.is_none());
    let old = running.worker_pid.unwrap();
    assert_eq!(unsafe { libc::kill(old as i32, 0) }, -1);
    fixture.terminate(first).await;
    let second = fixture.supervisor(&helper);
    let restored = fixture.status(|s| s.mode == Mode::Paused).await;
    assert_eq!(restored.phase, "paused");
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
    let failed = fixture.status(|s| s.phase == "state_error").await;
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
        paths
            .directory
            .join(format!("intent-{}.tmp", std::process::id())),
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
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("descendant").exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let descendant: i32 = fs::read_to_string(fixture.0.join("descendant"))
        .unwrap()
        .trim()
        .parse()
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

#[tokio::test]
async fn progress_reporter_fixture() {
    if std::env::var_os("TIMELORD_PROGRESS_FIXTURE").is_none() {
        return;
    }
    let progress = std::sync::Arc::new(timelord_supervisor::progress::Progress::default());
    let _reporter = timelord_supervisor::progress::Reporter::from_environment(progress).unwrap();
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test]
async fn independent_watchdog_reaps_and_replaces_a_stopped_worker_without_gui() {
    let fixture = Fixture::new();
    let executable = std::env::current_exe().unwrap();
    let helper = fixture.helper(&format!("export TIMELORD_PROGRESS_FIXTURE=1\nexec '{}' --exact progress_reporter_fixture --nocapture", executable.display()));
    let supervisor = fixture.supervisor(&helper);
    let first = fixture.status(|s| s.progress.is_some()).await;
    let pid = first.worker_pid.unwrap() as i32;
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGSTOP), 0);
    }
    let replacement = tokio::time::timeout(Duration::from_secs(55), async {
        loop {
            let status = request(&fixture.config(), Control::Status, None)
                .await
                .unwrap();
            if status.worker_pid.is_some_and(|next| next != pid as u32) {
                break status;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("watchdog did not replace its stopped worker");
    assert!(replacement.worker_generation > first.worker_generation);
    assert!(replacement.restarts >= 1);
    assert!(
        replacement
            .recoveries
            .last()
            .unwrap()
            .reason
            .contains("heartbeat"),
        "unexpected restart: {:?}",
        replacement.recoveries
    );
    unsafe {
        assert_eq!(
            libc::kill(pid, 0),
            -1,
            "old worker must be reaped before replacement"
        );
    }
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(supervisor).await;
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
    fixture.status(|s| s.phase == "recovering").await;
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
async fn fresh_status_readers_observe_durable_pause_before_cleanup_acknowledges() {
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
    let config = fixture.config();
    let pause = tokio::spawn(async move { request(&config, Control::Pause, None).await });
    let stopping = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.status(|s| s.phase == "stopping"),
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
    assert!(
        !pause.is_finished(),
        "Pause acknowledged before graceful cleanup"
    );
    let paused = pause.await.unwrap().unwrap();
    assert_eq!(paused.phase, "paused");
    assert!(paused.worker_pid.is_none());
    fixture.terminate(supervisor).await;
}

#[tokio::test]
async fn idempotent_resume_during_backoff_does_not_rewrite_intent() {
    let fixture = Fixture::new();
    let helper = fixture.helper("exit 17");
    let supervisor = fixture.supervisor(&helper);
    let failed = fixture.status(|s| s.phase == "recovering").await;
    let resumed = request(&fixture.config(), Control::Resume, Some(failed.revision))
        .await
        .unwrap();
    assert_eq!(resumed.revision, failed.revision);
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(supervisor).await;
}

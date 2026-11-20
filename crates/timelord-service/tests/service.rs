#![cfg(unix)]
//! Exercise the actual helper executable and public status contract without USB.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture {
    child: Child,
    directory: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn status(port: u16) -> Option<serde_json::Value> {
    get_json(port, "/v1/status")
}

fn get_json(port: u16, path: &str) -> Option<serde_json::Value> {
    let mut socket =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))
            .ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).ok()?;
    let (_, body) = response.split_once("\r\n\r\n")?;
    serde_json::from_str(body).ok()
}

fn wait_for(port: u16, clients: usize) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = status(port)
            && value["connected_client_count"] == clients
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "Status did not reach {clients} connected clients"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn helper_reports_real_socket_state_without_claiming_productivity() {
    let status_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let board_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let status_port = status_reservation.local_addr().unwrap().port();
    let board_port = board_reservation.local_addr().unwrap().port();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("timelord-mac-test-{}-{unique}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let config = directory.join("config.toml");
    std::fs::write(&config,format!("schema_version=1\n[service]\nstatus_listen=\"127.0.0.1:{status_port}\"\n[[hardware.boards]]\nid=\"fixture\"\nname=\"Fixture\"\nclient_port={board_port}\nprofile=\"manual\"\n")).unwrap();
    drop(status_reservation);
    drop(board_reservation);
    let child = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["serve", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut fixture = Fixture { child, directory };
    let before = wait_for(status_port, 0);
    assert_eq!(before["api_version"], 1);
    assert_eq!(before["production_enabled"], false);
    assert!(before["delivered_ips"].is_null());
    assert!(before["latest_proof_unix"].is_null());
    assert_eq!(before["full_node"]["state"], "not_connected");
    assert_eq!(before["boards"][0]["hardware_state"], "not_inspected");
    let probe = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["test-node", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!probe.status.success());
    assert!(String::from_utf8_lossy(&probe.stderr).contains("Pause Timelord"));

    assert_eq!(wait_for(status_port, 0)["service_pid"], fixture.child.id());
    let mut clients: Vec<_> = (0..3)
        .map(|_| TcpStream::connect(("127.0.0.1", board_port)).unwrap())
        .collect();
    wait_for(status_port, 3);
    let mut extra = TcpStream::connect(("127.0.0.1", board_port)).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    match extra.read(&mut [0]) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        result => panic!("Excess client was not rejected: {result:?}"),
    }
    assert_eq!(wait_for(status_port, 3)["production_enabled"], false);
    for client in &mut clients {
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client.write_all(b"STOP").unwrap();
        let mut ack = [0; 3];
        client.read_exact(&mut ack).unwrap();
        assert_eq!(&ack, b"ACK");
    }
    wait_for(status_port, 0);
    let output = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["status", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(output.status.success());
    let via_cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(via_cli["service_pid"], fixture.child.id());
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &fixture.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(exit) = fixture.child.try_wait().unwrap() {
            assert!(exit.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Helper did not shut down gracefully"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let checkpoint = config.with_extension("history.json");
    assert!(
        checkpoint.is_file(),
        "Graceful shutdown must flush chart history"
    );
    // Seed only the fixture's historical chart file, not a live proof stream.
    // The restarted helper must expose it without claiming current productivity.
    let configured: timelord_core::Config =
        toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    let scope = serde_json::to_vec(&(
        configured.node.network.clone(),
        &configured.node.host,
        configured.node.port,
        configured.assignments().unwrap(),
    ))
    .unwrap();
    let scope: String = dg_xch_core::utils::hash_256(scope)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let saved = serde_json::json!({"schema_version":1,"scope":scope,
        "capture_intervals":[{"start_unix":now-100,"end_unix":now-30}],
        "observations":[{"id":50,"chain":"challenge","timestamp_unix":now-60,"work_started_unix":now-61,"ips":425000}],
        "events":[{"id":22,"timestamp_unix":now-60,"board_id":"fixture","kind":"recovery","end_unix":null}]});
    std::fs::write(&checkpoint, serde_json::to_vec(&saved).unwrap()).unwrap();
    fixture.child = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["serve", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let restarted = wait_for(status_port, 0);
    assert!(restarted["latest_proof_unix"].is_null());
    assert!(restarted["delivered_ips"].is_null());
    assert_eq!(restarted["proofs_delivered"], 0);
    let history = get_json(status_port, "/v3/history").unwrap();
    assert_eq!(history["api_version"], 3);
    assert_eq!(history["service_pid"], fixture.child.id());
    assert!(
        history["estimated_observations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Old delivery measurements remain on disk, never masquerading as new IPS.
    assert_eq!(history["events"][0]["kind"], "recovery");
    assert_eq!(history["events"][1]["kind"], "telemetry_gap");
    assert_eq!(history["events"][1]["timestamp_unix"], now - 30);
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &fixture.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while fixture.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "Restarted fixture did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    let flushed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(checkpoint).unwrap()).unwrap();
    assert_eq!(flushed["observations"][0]["id"], 50);
}

fn raw_get(port: u16, path: &str) -> (u16, String, String) {
    let mut socket =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    let (header, body) = response.split_once("\r\n\r\n").unwrap();
    (
        header.split_whitespace().nth(1).unwrap().parse().unwrap(),
        header.to_owned(),
        body.to_owned(),
    )
}
fn observed_fixture() -> (Fixture, u16, u16) {
    let status = TcpListener::bind("127.0.0.1:0").unwrap();
    let observe = TcpListener::bind("127.0.0.1:0").unwrap();
    let status_port = status.local_addr().unwrap().port();
    let observe_port = observe.local_addr().unwrap().port();
    let directory = std::env::temp_dir().join(format!(
        "observability-http-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let config = directory.join("config.toml");
    std::fs::write(&config,format!("[service]\nstatus_listen='127.0.0.1:{status_port}'\nobservability_listen='127.0.0.1:{observe_port}'\n")).unwrap();
    drop(status);
    drop(observe);
    let child = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["serve", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let fixture = Fixture { child, directory };
    wait_for(status_port, 0);
    (fixture, status_port, observe_port)
}
#[test]
fn deployment_listener_is_read_only_serves_metrics_while_unready_and_shuts_down_cleanly() {
    let (mut fixture, status, observe) = observed_fixture();
    for port in [status, observe] {
        let (code, _, body) = raw_get(port, "/healthz");
        assert_eq!(code, 200);
        assert_eq!(body, "Ok\n");
        let (code, _, body) = raw_get(port, "/readyz");
        assert_eq!(code, 503);
        assert!(body.contains("setup_required"));
        let (code, headers, body) = raw_get(port, "/metrics");
        assert_eq!(code, 200);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: text/plain; version=0.0.4")
        );
        assert!(
            body.lines()
                .any(|line| line.starts_with("chia_timelord_version{")
                    && line.contains("network=\"mainnet\""))
        );
        assert!(body.contains("chia_timelord_proofs_verified_total"));
        assert!(!body.contains("chia_timelord_estimated_ips{"));
    }
    for path in ["/v1/status", "/v3/history", "/health"] {
        assert_eq!(raw_get(observe, path).0, 404);
    }
    assert_eq!(raw_get(status, "/health").0, 200);
    unsafe {
        assert_eq!(libc::kill(fixture.child.id() as i32, libc::SIGTERM), 0);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(TcpStream::connect(("127.0.0.1", status)).is_err());
    assert!(TcpStream::connect(("127.0.0.1", observe)).is_err());
}
#[test]
fn occupied_observability_port_fails_startup_with_a_clear_error() {
    let status = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = status.local_addr().unwrap().port();
    let other = occupied.local_addr().unwrap().port();
    let directory = std::env::temp_dir().join(format!("observability-bind-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let config = directory.join("config.toml");
    std::fs::write(&config,format!("[service]\nstatus_listen='127.0.0.1:{port}'\nobservability_listen='127.0.0.1:{other}'\n")).unwrap();
    drop(status);
    let result = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["serve", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Binding observability endpoint"));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn supervised_worker_exits_when_its_supervisor_lifetime_pipe_closes() {
    use std::os::unix::process::CommandExt;
    let directory = PathBuf::from(format!(
        "/tmp/tl-parent-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config = directory.join("config.toml");
    std::fs::write(
        &config,
        format!("[service]\nstatus_listen='127.0.0.1:{port}'\n"),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["serve", "--supervised", "--config"])
        .arg(&config)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lifetime = child.stdin.take().unwrap();
    let mut fixture = Fixture { child, directory };
    let deadline = Instant::now() + Duration::from_secs(5);
    while status(port).is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(lifetime);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(exit) = fixture.child.try_wait().unwrap() {
            assert!(exit.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not stop on supervisor EOF"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn supervised_startup_reports_discovery_before_a_slow_driver_returns() {
    use std::os::unix::net::UnixDatagram;
    use timelord_supervisor::progress::{Snapshot, Stage};
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = PathBuf::from("/private/tmp")
        .join(format!("tl-startup-probe-{}-{unique}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("fake-driver.c");
    let driver = directory.join("fake-driver.dylib");
    // Only these fake enumeration exports are loaded; no USB device is opened.
    std::fs::write(&source, "#include <stdint.h>\n#include <unistd.h>\nuint32_t FT_CreateDeviceInfoList(uint32_t *n) { sleep(30); *n=0; return 0; }\nuint32_t FT_GetDeviceInfoList(void *nodes, uint32_t *n) { return 0; }\n").unwrap();
    assert!(
        Command::new("cc")
            .arg("-dynamiclib")
            .arg(&source)
            .arg("-o")
            .arg(&driver)
            .status()
            .unwrap()
            .success()
    );
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = timelord_core::Config::default();
    config.service.status_listen = reservation.local_addr().unwrap();
    config.hardware.client_binary = Some("/usr/bin/true".into());
    config.hardware.driver_library = Some(driver);
    config.validate().unwrap();
    let path = directory.join("config.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let progress_path = directory.join("progress.sock");
    let progress = UnixDatagram::bind(&progress_path).unwrap();
    progress
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    drop(reservation);
    let child = Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["serve", "--config"])
        .arg(&path)
        .env("TIMELORD_SUPERVISOR_PROGRESS", &progress_path)
        .env("TIMELORD_SUPERVISOR_GENERATION", "7")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut fixture = Fixture { child, directory };
    let mut sequence = None;
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut bytes = [0u8; 8192];
    loop {
        let size = progress
            .recv(&mut bytes)
            .expect("missing startup heartbeat");
        let report: Snapshot = serde_json::from_slice(&bytes[..size]).unwrap();
        assert_eq!(report.worker_pid, fixture.child.id());
        assert_eq!(report.generation, 7);
        assert!(!report.ready);
        if report
            .work
            .iter()
            .flatten()
            .any(|work| work.stage == Stage::Discovery)
        {
            if let Some(previous) = sequence {
                assert!(report.sequence > previous);
                break;
            }
            sequence = Some(report.sequence);
        }
        assert!(
            Instant::now() < deadline,
            "USB startup was not reported as discovery"
        );
    }
    assert!(fixture.child.try_wait().unwrap().is_none());
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &fixture.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(exit) = fixture.child.try_wait().unwrap() {
            assert!(exit.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "startup cancellation did not stop the fixture"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

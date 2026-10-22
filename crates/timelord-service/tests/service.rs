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
        configured.node.network,
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

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
    let mut socket =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))
            .ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    socket
        .write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
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
}

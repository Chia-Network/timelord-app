use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
use timelord_core::Network;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test]
async fn requested_shutdown_prevents_startup_from_binding_or_opening_hardware() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    config.service.status_listen = occupied.local_addr().unwrap();
    serve(config, PathBuf::new(), async {}).await.unwrap();
}
#[tokio::test]
async fn startup_sigterm_is_retained_until_shutdown_is_polled() {
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    assert_eq!(
        unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) },
        0
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    tokio::time::timeout(Duration::from_secs(1), shutdown_signal_from(term))
        .await
        .unwrap();
}

#[tokio::test]
async fn topology_generations_release_owners_keep_status_and_preserve_faults() {
    let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
    let process = operational::State::new(metrics.clone());
    metrics.topology_changed();
    // All ports and files belong to this fixture; no node, native binary or USB.
    let dir = std::env::temp_dir().join(format!(
        "topology-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.service.control_socket = Some(dir.join("control.sock"));
    let mut reserved = vec![];
    for i in 0..3 {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.hardware.boards.push(toml::from_str(&format!("id='b{i}'\nname='B{i}'\nserial='s{i}'\nprofile='manual'\ndevice_location={}\nclient_port={}", i+1, port.local_addr().unwrap().port())).unwrap());
        reserved.push(port);
    }
    let mut faults = std::collections::HashMap::new();
    faults.insert(
        "b2".into(),
        runtime::HardwareState {
            latched_fault: true,
            state: "thermal_stop".into(),
            ..Default::default()
        },
    );
    drop(reserved);
    let mut run = start_run(
        config.clone(),
        hardware::prepare(&config).await.unwrap(),
        dir.join("history.json"),
        &faults,
        unix_now() - 3600,
        process.clone(),
    )
    .await
    .unwrap();
    let service_started_unix = run.shared.service_started_unix;
    let (current, state) = watch::channel(run.shared.clone());
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = http.local_addr().unwrap();
    let http_process = run.shared.process.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            http,
            status_api::router(observability::HttpState {
                current: state,
                process: http_process,
            }),
        )
        .await
        .unwrap();
    });
    for indices in [vec![0], vec![], vec![0, 1], vec![0, 1, 2]] {
        let mut socket = if let Some(board) = run.shared.config.hardware.boards.first() {
            let socket = TcpStream::connect(("127.0.0.1", board.client_port))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while run.shared.clients.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            Some(socket)
        } else {
            None
        };
        let devices = indices
            .iter()
            .map(|i| inventory::Device {
                name: "Chia VDF A".into(),
                location: i + 1,
                serial: Some(format!("s{i}")),
            })
            .collect();
        let next = inventory::connected_plan(run.shared.config.clone(), devices).unwrap();
        run.stop().await.unwrap();
        if let Some(ref mut socket) = socket {
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        }
        assert!(!dir.join("control.sock").exists());
        run = start_run(
            next.clone(),
            hardware::prepare(&next).await.unwrap(),
            dir.join("history.json"),
            &faults,
            service_started_unix,
            process.clone(),
        )
        .await
        .unwrap();
        current.send_replace(run.shared.clone());
        assert_eq!(metrics.counter("topology_changes_total", &[]), 1);
        assert!(run.shared.runtime.lock().unwrap().boards["b2"].latched_fault);
        assert_eq!(
            run.shared.assignments.len(),
            if indices.is_empty() { 0 } else { 3 }
        );
        let mut status_socket = TcpStream::connect(address).await.unwrap();
        status_socket
            .write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        status_socket.read_to_string(&mut response).await.unwrap();
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["boards"].as_array().unwrap().len(), indices.len());
        assert_eq!(value["connected_client_count"], 0);
        assert_eq!(value["proofs_delivered"], 0);
        assert_eq!(value["service_started_unix"], service_started_unix);
        assert!(value["started_unix"].as_u64().unwrap() >= service_started_unix + 3600);
    }
    run.stop().await.unwrap();
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn unexpected_supervisor_exit_is_latched_as_unhealthy() {
    let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
    let process = operational::State::new(metrics.clone());
    let mut tasks = JoinSet::new();
    tasks.spawn(async { Ok(()) });
    essential_exit(&mut tasks, &process).await;
    assert!(!process.snapshot().healthy());
    // Creating a replacement runtime cannot clear the process health latch.
    metrics.begin_session();
    assert!(!process.snapshot().healthy());
}
#[tokio::test]
async fn observability_failures_leave_client_service_running() {
    for failure in ["io", "panic", "early_success"] {
        let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
        let process = operational::State::new(metrics.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            native: None,
            config: Config::default(),
            assignments: vec![],
            clients: Arc::new(Mutex::new(BTreeSet::new())),
            process: process.clone(),
            runtime: Arc::new(Mutex::new(runtime::RuntimeState::new(false))),
            started_unix: unix_now(),
            service_started_unix: unix_now(),
        });
        let (stop, rx) = watch::channel(false);
        let mut client = tokio::spawn(client_listener(listener, "a".into(), 1, shared, rx, None));
        let mut observer = Some(tokio::spawn(async move {
            match failure {
                "io" => Err(std::io::Error::other("fixture listener failure")),
                "panic" => panic!("fixture listener panic"),
                _ => Ok(()),
            }
        }));
        tokio::select! {
            _ = observation_exit(&mut observer, &process) => {},
            result = &mut client => panic!("Client service stopped: {result:?}"),
        }
        assert!(observer.is_none());
        assert!(!process.snapshot().healthy());
        metrics.begin_session();
        assert!(!process.snapshot().healthy());
        // The independent client listener still handles protocol traffic.
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(b"STOP").await.unwrap();
        let mut ack = [0; 3];
        tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut ack))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&ack, b"ACK");
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
#[tokio::test]
async fn idle_clients_stop_cleanly_and_release_their_slot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shared = Arc::new(Shared {
        native: None,
        config: Config::default(),
        assignments: vec![],
        clients: Arc::new(Mutex::new(BTreeSet::new())),
        process: operational::State::fixture(),
        runtime: Default::default(),
        started_unix: unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(client_listener(
        listener,
        "a".into(),
        1,
        shared.clone(),
        rx,
        None,
    ));
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(b"STOP").await.unwrap();
    let mut ack = [0; 3];
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut ack))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ack, b"ACK");
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(shared.clients.lock().unwrap().is_empty());
}

#[tokio::test]
async fn transient_accept_errors_keep_client_listener_available() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shared = Arc::new(Shared {
        native: None,
        config: Config::default(),
        assignments: vec![],
        clients: Arc::new(Mutex::new(BTreeSet::new())),
        process: operational::State::fixture(),
        runtime: Default::default(),
        started_unix: unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let (stop, rx) = watch::channel(false);
    let started = std::time::Instant::now();
    let mut task = tokio::spawn(client_listener_with(
        AcceptBackoff::with_injected([
            std::io::Error::from_raw_os_error(libc::ECONNABORTED),
            std::io::Error::from_raw_os_error(libc::EMFILE),
        ]),
        listener,
        "a".into(),
        1,
        shared.clone(),
        rx,
        None,
    ));
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(b"STOP").await.unwrap();
    let mut ack = [0; 3];
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut ack))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ack, b"ACK");
    assert!(started.elapsed() >= Duration::from_millis(140));
    assert!(!task.is_finished());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), &mut task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(shared.clients.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_accept_backoff_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let shared = Arc::new(Shared {
        native: None,
        config: Config::default(),
        assignments: vec![],
        clients: Arc::new(Mutex::new(BTreeSet::new())),
        process: operational::State::fixture(),
        runtime: Default::default(),
        started_unix: unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let (stop, rx) = watch::channel(false);
    let mut task = tokio::spawn(client_listener_with(
        AcceptBackoff::with_injected(
            (0..20).map(|_| std::io::Error::from_raw_os_error(libc::ECONNABORTED)),
        ),
        listener,
        "a".into(),
        1,
        shared,
        rx,
        None,
    ));
    // Let the listener record an injected failure and enter its retry wait.
    tokio::time::advance(Duration::from_millis(1)).await;
    stop.send(true).unwrap();
    // The pending retry is at least 50ms away in virtual time; a shutdown that
    // waited for it would not complete before this deadline.
    tokio::time::timeout(Duration::from_millis(10), &mut task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

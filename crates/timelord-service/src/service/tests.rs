use super::test_support::{fixture_dir, run_from};
use super::*;
use timelord_core::Network;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;

fn shared() -> Arc<Shared> {
    Arc::new(crate::Shared::new(
        None,
        Config::default(),
        Arc::new(Mutex::new(runtime::RuntimeState::new(false))),
        operational::State::fixture(),
        unix_now(),
        unix_now(),
    ))
}
async fn identified(address: std::net::SocketAddr, engine: u8) -> TcpStream {
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(format!("TLENGINE 1 {engine}\n").as_bytes())
        .await
        .unwrap();
    socket
}
#[tokio::test]
async fn requested_shutdown_prevents_startup_from_binding_or_opening_hardware() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::default();
    config.service.status_listen = occupied.local_addr().unwrap();
    serve(config, PathBuf::new(), PathBuf::new(), false, async {})
        .await
        .unwrap();
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
async fn owners_and_routes_survive_inventory_updates_and_a_node_profile_switch() {
    let dir = fixture_dir("node-switch");
    let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
    let process = operational::State::new(metrics.clone());
    let mut config = Config::default();
    for i in 0..2 {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.hardware.boards.push(toml::from_str(&format!("id='b{i}'\nname='B{i}'\nserial='s{i}'\nprofile='manual'\ndevice_location={}\nclient_port={}", i+1, port.local_addr().unwrap().port())).unwrap());
    }
    let safety = Arc::new(safety::Store::open(&dir.join("config.toml")).unwrap());
    let (node_requests, _node_reloads) = tokio::sync::mpsc::channel(1);
    let mut run = start_run(
        config.clone(),
        None,
        dir.join("history.json"),
        safety,
        unix_now(),
        process.clone(),
        node_requests,
    )
    .await
    .unwrap();
    let (current, _state) = watch::channel(run.shared.clone());
    // Without a native client every owner holds with an explanation.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let held = {
                let state = run.shared.runtime.lock().unwrap();
                config.hardware.boards.iter().all(|b| {
                    state.boards.get(&b.id).is_some_and(|h| {
                        h.recovery_reason
                            .as_deref()
                            .is_some_and(|r| r.contains("native ASIC client"))
                    })
                })
            };
            if held {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    {
        let mut state = run.shared.runtime.lock().unwrap();
        state
            .boards
            .get_mut("b0")
            .unwrap()
            .lifecycle
            .testing()
            .fault(
                crate::board_lifecycle::Fault::Thermal,
                "fixture".into(),
                unix_now(),
            );
        state.estimated(timelord_core::Chain::Challenge, 1.0e6, 1, "b1", 1);
        state.proofs_delivered = 5;
        state.failed_proofs = 1;
    }
    // A relocated board is the same owner: nothing is retired.
    let mut moved = run.shared.config.clone();
    moved.hardware.boards[1].device_location = Some(9);
    run.publish(moved, &current).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!run.tasks.is_empty());
    // A node profile switch keeps board state, restrictions and routes.
    let mut testnet = run.shared.config.clone();
    testnet.node_mut().network = Network::Testnet11;
    testnet.node_mut().port = 58444;
    run.replace_node(testnet, &current).await.unwrap();
    {
        let state = run.shared.runtime.lock().unwrap();
        assert!(state.boards["b0"].lifecycle.status().latched_fault);
        assert!(state.events.iter().any(|e| e.kind == "node_profile_change"));
        // Chain measurements from the old node never join the new chart.
        assert_eq!(state.estimated_observations.iter().count(), 0);
        assert_eq!(state.node, "connecting");
        // Proofs sent to the previous node never count for the new one.
        assert_eq!((state.proofs_delivered, state.failed_proofs), (0, 0));
    }
    assert_eq!(run.shared.config.node().network, Network::Testnet11);
    let exposition = metrics.render(&run.shared, unix_now()).unwrap();
    assert!(exposition.contains("network=\"testnet11\""));
    assert!(!exposition.contains("network=\"mainnet\""));
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn same_scope_node_reload_keeps_the_restored_chart_archive() {
    let dir = fixture_dir("node-reload-archive");
    let history = dir.join("history.json");
    let config = Config::default();
    let now = unix_now();
    let mut seeded = runtime::RuntimeState::new(false);
    seeded.estimated(timelord_core::Chain::Challenge, 1.0e6, now, "b0", 0);
    let store = history_store::Store::new(history.clone(), &config).unwrap();
    store
        .save(&store.capture(&seeded, now, now).pruned(now))
        .unwrap();
    drop(store);
    let (node_requests, _node_reloads) = tokio::sync::mpsc::channel(1);
    let mut run = start_run(
        config,
        None,
        history,
        Arc::new(safety::Store::open(&dir.join("config.toml")).unwrap()),
        unix_now(),
        operational::State::fixture(),
        node_requests,
    )
    .await
    .unwrap();
    let (current, _state) = watch::channel(run.shared.clone());
    assert!(run.shared.runtime.lock().unwrap().archive.is_some());
    // Certificate-only reloads stay in the same history scope.
    let mut reloaded = run.shared.config.clone();
    reloaded.node_mut().certificate = Some(dir.join("new.cert"));
    reloaded.node_mut().ca_certificate = Some(dir.join("new.ca"));
    run.replace_node(reloaded, &current).await.unwrap();
    assert!(run.shared.runtime.lock().unwrap().archive.is_some());
    // A different node is a new context: the old archive must not join it.
    let mut moved = run.shared.config.clone();
    moved.node_mut().host = "other-node".into();
    run.replace_node(moved, &current).await.unwrap();
    assert!(run.shared.runtime.lock().unwrap().archive.is_none());
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn engine_sockets_need_a_unique_identity_and_reach_their_board_pool() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shared = shared();
    let (offer, pool) = crate::asic::pair();
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(client_listener(
        listener,
        "a".into(),
        shared.clone(),
        rx,
        offer,
    ));
    // An unidentified socket is closed without claiming a slot.
    let mut anonymous = TcpStream::connect(address).await.unwrap();
    anonymous.write_all(b"STOP").await.unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), anonymous.read(&mut byte))
            .await
            .unwrap()
            .unwrap_or(0),
        0
    );
    let _first = identified(address, 1).await;
    let lease = tokio::time::timeout(Duration::from_secs(2), pool.receive(Some(1), 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((lease.board.as_str(), lease.slot), ("a", 1));
    // A duplicate engine identity is rejected while the first is held.
    let mut duplicate = identified(address, 1).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), duplicate.read(&mut byte))
            .await
            .unwrap()
            .unwrap_or(0),
        0
    );
    assert_eq!(shared.clients.lock().unwrap().len(), 1);
    drop(lease);
    assert!(shared.clients.lock().unwrap().is_empty());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn observability_failures_leave_client_service_running() {
    for failure in ["io", "panic", "early_success"] {
        let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
        let process = operational::State::new(metrics.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(crate::Shared::new(
            None,
            Config::default(),
            Arc::new(Mutex::new(runtime::RuntimeState::new(false))),
            process.clone(),
            unix_now(),
            unix_now(),
        ));
        let (offer, pool) = crate::asic::pair();
        let (stop, rx) = watch::channel(false);
        let mut client = tokio::spawn(client_listener(listener, "a".into(), shared, rx, offer));
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
        // The independent client listener still accepts engine sockets.
        let _socket = identified(address, 0).await;
        tokio::time::timeout(Duration::from_secs(2), pool.receive(Some(0), 0))
            .await
            .unwrap()
            .unwrap();
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
#[tokio::test]
async fn transient_accept_errors_keep_client_listener_available() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shared = shared();
    let (offer, pool) = crate::asic::pair();
    let (stop, rx) = watch::channel(false);
    let started = std::time::Instant::now();
    let mut task = tokio::spawn(client_listener_with(
        AcceptBackoff::with_injected([
            std::io::Error::from_raw_os_error(libc::ECONNABORTED),
            std::io::Error::from_raw_os_error(libc::EMFILE),
        ]),
        listener,
        "a".into(),
        shared.clone(),
        rx,
        offer,
    ));
    let _socket = identified(address, 2).await;
    let lease = tokio::time::timeout(Duration::from_secs(2), pool.receive(Some(2), 0))
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(140));
    assert!(!task.is_finished());
    drop(lease);
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
    let (offer, _pool) = crate::asic::pair();
    let (stop, rx) = watch::channel(false);
    let mut task = tokio::spawn(client_listener_with(
        AcceptBackoff::with_injected(
            (0..20).map(|_| std::io::Error::from_raw_os_error(libc::ECONNABORTED)),
        ),
        listener,
        "a".into(),
        shared(),
        rx,
        offer,
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

#[tokio::test]
async fn no_boards_wait_for_hardware_without_claiming_node_failure() {
    for configured in [false, true] {
        let dir = fixture_dir(&format!("waiting-{configured}"));
        let mut config = Config::default();
        if configured {
            config.hardware.boards.push(
                toml::from_str(
                    "id='a'\nname='A'\nserial='fixture'\nprofile='manual'\nclient_port=0",
                )
                .unwrap(),
            );
        }
        let (node_requests, _node_reloads) = tokio::sync::mpsc::channel(1);
        let run = start_run(
            config,
            None,
            dir.join("history.json"),
            Arc::new(safety::Store::open(&dir.join("config.toml")).unwrap()),
            unix_now(),
            operational::State::fixture(),
            node_requests,
        )
        .await
        .unwrap();
        {
            let state = run.shared.runtime.lock().unwrap();
            assert_eq!(state.phase, "waiting_for_clients");
            assert_eq!(state.node, "waiting_for_hardware");
            assert_eq!(
                state.message,
                if configured {
                    "Starting ASIC boards before connecting to the full node."
                } else {
                    "Connect an ASIC board to resume."
                }
            );
        }
        run.stop().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
fn automatic_board() -> timelord_core::BoardConfig {
    toml::from_str("id='a'\nname='A'\nserial='auto'\nprofile='automatic'\nclient_port=0").unwrap()
}
fn lost_journal(name: &str) -> (PathBuf, PathBuf, PathBuf, Config) {
    let dir = fixture_dir(name);
    let config_path = dir.join("config.toml");
    let history_path = dir.join("history.json");
    crate::safety::Store::open(&config_path).unwrap();
    let journal = timelord_supervisor::Paths::for_config(&config_path)
        .unwrap()
        .directory
        .join("board-safety.json");
    std::fs::remove_file(&journal).unwrap();
    assert_eq!(
        crate::safety::Store::inspect(&config_path)
            .unwrap()
            .journal_reset(),
        Some(crate::safety::JournalReset::Missing)
    );
    let mut config = Config::default();
    config.service.status_listen = "127.0.0.1:0".parse().unwrap();
    (dir, config_path, history_path, config)
}
fn assert_journal_still_lost(dir: &std::path::Path, config_path: &std::path::Path) {
    let journal = timelord_supervisor::Paths::for_config(config_path)
        .unwrap()
        .directory
        .join("board-safety.json");
    assert_eq!(
        crate::safety::Store::inspect(config_path)
            .unwrap()
            .journal_reset(),
        Some(crate::safety::JournalReset::Missing)
    );
    assert!(!journal.exists());
    assert_eq!(
        crate::safety::Store::open(config_path)
            .unwrap()
            .journal_reset(),
        Some(crate::safety::JournalReset::Missing)
    );
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(dir);
}
#[tokio::test]
async fn a_prepare_failure_leaves_a_lost_safety_journal_absent() {
    let (dir, config_path, history_path, mut config) = lost_journal("lost-prepare");
    config.hardware.boards.push(automatic_board());
    let error = serve(
        config,
        history_path,
        config_path.clone(),
        false,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("Automatic mode requires a native client"),
        "{error:#}"
    );
    assert_journal_still_lost(&dir, &config_path);
}
#[tokio::test]
async fn a_control_socket_file_leaves_a_lost_safety_journal_absent() {
    let (dir, config_path, history_path, mut config) = lost_journal("lost-control");
    let socket = dir.join("control.sock");
    std::fs::write(&socket, b"not a socket").unwrap();
    config.service.control_socket = Some(socket);
    let error = serve(
        config,
        history_path,
        config_path.clone(),
        false,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("Private control path is not an owned socket"),
        "{error:#}"
    );
    assert_journal_still_lost(&dir, &config_path);
}
#[tokio::test]
async fn shutdown_during_prepare_leaves_a_lost_safety_journal_absent() {
    let (dir, config_path, history_path, mut config) = lost_journal("lost-shutdown");
    let script = dir.join("slow-client");
    crate::test_scripts::write_executable_script(&script, "#!/bin/sh\nexec sleep 2\n");
    config.hardware.client_binary = Some(script);
    config.hardware.boards.push(automatic_board());
    serve(
        config,
        history_path,
        config_path.clone(),
        false,
        tokio::time::sleep(Duration::from_millis(500)),
    )
    .await
    .unwrap();
    assert_journal_still_lost(&dir, &config_path);
}
async fn loopback_status() -> std::net::SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

fn discovered_devices(count: u32) -> Vec<crate::inventory::Device> {
    (1..=count)
        .map(|index| crate::inventory::Device {
            name: format!("ASIC {index}"),
            location: index,
            serial: Some(format!("TL-TEST-{index}")),
        })
        .collect()
}

async fn wait_for(path: &std::path::Path, task: &mut tokio::task::JoinHandle<Result<()>>) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if path.exists() {
            return true;
        }
        if task.is_finished() {
            task.await
                .expect("serve task")
                .expect("serve should keep running until shutdown");
            return false;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn probe_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

/// Wait for `count` probe lines under paused time. Advances virtual time in
/// 1ms steps so the I/O driver is polled, without sleeping on the wall clock.
async fn wait_for_probe_lines(
    path: &std::path::Path,
    count: usize,
    task: &mut tokio::task::JoinHandle<Result<()>>,
    budget: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let have = probe_lines(path);
        if have >= count {
            return have;
        }
        if task.is_finished() || tokio::time::Instant::now() >= deadline {
            return have;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;
    }
}

async fn yield_paused(steps: usize) {
    for _ in 0..steps {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn shutdown_during_discovered_prepare_returns_promptly() {
    let dir = fixture_dir("discover-shutdown");
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
root=$(dirname "$0")
echo started >> "$root/started"
exec sleep 30
"#,
    );
    crate::inventory::fixture::set(binary.clone(), discovered_devices(3));
    let mut config = Config::default();
    config.service.status_listen = loopback_status().await;
    config.hardware.client_binary = Some(binary);
    let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(serve(
        config,
        dir.join("history.json"),
        dir.join("config.toml"),
        false,
        async {
            let _ = shutdown.await;
        },
    ));
    assert!(
        wait_for(&dir.join("started"), &mut task).await,
        "discovered Automatic boards should start a capability probe"
    );
    let started = std::time::Instant::now();
    let _ = signal.send(());
    tokio::time::timeout(Duration::from_secs(1), &mut task)
        .await
        .expect("shutdown must not wait for the capability probe")
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "serve must stay responsive while a discovered prepare is in flight"
    );
    assert!(
        !dir.join("launches-1").exists(),
        "Automatic clients must not start without prepared capabilities"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn discovered_prepare_failure_backs_off_before_retrying() {
    let dir = fixture_dir("discover-backoff");
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
root=$(dirname "$0")
printf 'probe
' >> "$root/probes"
exit 1
"#,
    );
    crate::inventory::fixture::set(binary.clone(), discovered_devices(3));
    let mut config = Config::default();
    config.service.status_listen = loopback_status().await;
    config.hardware.client_binary = Some(binary);
    let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(serve(
        config,
        dir.join("history.json"),
        dir.join("config.toml"),
        false,
        async {
            let _ = shutdown.await;
        },
    ));
    let probes = dir.join("probes");
    assert_eq!(
        wait_for_probe_lines(&probes, 1, &mut task, Duration::from_secs(1)).await,
        1,
        "the first discovered prepare should run"
    );
    // Let the serve loop observe the failure and start the backoff before
    // virtual time jumps; otherwise retry_at is stamped after the jump.
    yield_paused(64).await;
    tokio::time::advance(DISCOVERY_PREPARE_RETRY - Duration::from_secs(1)).await;
    yield_paused(64).await;
    assert_eq!(
        probe_lines(&probes),
        1,
        "a failed prepare must not re-probe during backoff"
    );
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(
        wait_for_probe_lines(&probes, 2, &mut task, Duration::from_secs(2)).await,
        2,
        "prepare should run again after the backoff"
    );
    let _ = signal.send(());
    yield_paused(32).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    yield_paused(32).await;
    let result = tokio::time::timeout(Duration::from_secs(1), &mut task)
        .await
        .expect("shutdown after backoff");
    result.unwrap().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

async fn automatic_facts(dir: &std::path::Path) -> (Config, Arc<hardware::Facts>) {
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
case "$1" in
 --control-version) echo timelord-live-control-v1; exit 0;;
 --control-frequencies) echo '[1450,1460]'; exit 0;;
esac
exit 1
"#,
    );
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary);
    config.hardware.boards = (1..=3)
        .map(|index| {
            toml::from_str(&format!(
                "id='usb-{index}'
name='ASIC {index}'
serial='TL-TEST-{index}'
client_port={}
device_location={index}
profile='automatic'",
                18110 + index
            ))
            .unwrap()
        })
        .collect();
    let native = hardware::prepare(&config)
        .await
        .unwrap()
        .expect("prepared capabilities");
    (config, native)
}

fn set_live(
    run: &mut Run,
    config: Config,
    native: Option<Arc<hardware::Facts>>,
    current: &watch::Sender<Arc<Shared>>,
) {
    let previous = &run.shared;
    let updated = Arc::new(Shared {
        config,
        native,
        clients: previous.clients.clone(),
        runtime: previous.runtime.clone(),
        process: previous.process.clone(),
        scope: previous.scope,
        started_unix: previous.started_unix,
        service_started_unix: previous.service_started_unix,
    });
    run.owner_updates.send_replace(updated.clone());
    run.shared = updated.clone();
    current.send_replace(updated);
}

#[tokio::test]
async fn replace_node_during_prepare_keeps_the_new_node() {
    let dir = fixture_dir("prepare-keeps-node");
    let (discovered, native) = automatic_facts(&dir).await;
    let mut config = Config::default();
    config.hardware.client_binary = discovered.hardware.client_binary.clone();
    let (mut run, current) = run_from(&dir, config).await;
    let mut testnet = run.shared.config.clone();
    testnet.node_mut().network = Network::Testnet11;
    testnet.node_mut().port = 58444;
    run.replace_node(testnet, &current).await.unwrap();
    // A prepare started against the original mainnet + board snapshot completes.
    run.adopt_prepared_native(Some(native), &current).unwrap();
    assert_eq!(run.shared.config.node().network, Network::Testnet11);
    assert_eq!(run.shared.config.node().port, 58444);
    assert!(
        run.shared.config.hardware.boards.is_empty(),
        "prepared snapshot boards must not be published"
    );
    assert!(run.shared.native.is_some());
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn boards_unpublished_during_prepare_stay_unpublished() {
    let dir = fixture_dir("prepare-keeps-unpub");
    let (discovered, native) = automatic_facts(&dir).await;
    let mut empty = Config::default();
    empty.hardware.client_binary = discovered.hardware.client_binary.clone();
    let (mut run, current) = run_from(&dir, empty.clone()).await;
    set_live(&mut run, discovered, None, &current);
    assert_eq!(run.shared.config.hardware.boards.len(), 3);
    run.publish(empty, &current).unwrap();
    assert!(run.shared.config.hardware.boards.is_empty());
    run.adopt_prepared_native(Some(native), &current).unwrap();
    assert!(
        run.shared.config.hardware.boards.is_empty(),
        "completing prepare must not re-add unpublished boards"
    );
    assert!(run.shared.native.is_some());
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn check_hardware_refuses_after_a_failed_start_on_a_lost_journal() {
    let (dir, config_path, history_path, mut config) = lost_journal("lost-check-hardware");
    config.hardware.boards.push(automatic_board());
    let error = serve(
        config,
        history_path,
        config_path.clone(),
        false,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("Automatic mode requires a native client"),
        "{error:#}"
    );
    let error = crate::diagnostics::prepare_store(&config_path)
        .map(|_| ())
        .expect_err("check-hardware must refuse a lost journal");
    let text = format!("{error:#}");
    assert!(text.contains("missing or unreadable"), "{text}");
    assert!(text.contains("start the worker"), "{text}");
    assert!(text.contains("saved board state"), "{text}");
    assert_journal_still_lost(&dir, &config_path);
}
#[tokio::test]
async fn reload_node_accepts_only_saved_node_changes_from_the_real_config_file() {
    let dir = fixture_dir("reload-node");
    let path = dir.join("config.toml");
    let mut config = Config::default();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.hardware.boards.push(
        toml::from_str(&format!(
            "id='b0'\nname='B0'\nserial='rn0'\nprofile='manual'\nclient_port={}",
            port.local_addr().unwrap().port()
        ))
        .unwrap(),
    );
    drop(port);
    crate::save(&path, &config).unwrap();
    // Start exactly as `serve` does.
    let mut running = crate::load(&path).unwrap();
    running.service.control_socket = Some(crate::automatic::socket_path(&path, &running));
    crate::set_default_automatic_state(&mut running, &path).unwrap();
    let (node_requests, _node_reloads) = tokio::sync::mpsc::channel(1);
    let mut run = start_run(
        hardware::continuous::known_configuration(running),
        None,
        dir.join("history.json"),
        Arc::new(safety::Store::open(&path).unwrap()),
        unix_now(),
        operational::State::fixture(),
        node_requests,
    )
    .await
    .unwrap();
    let (current, _state) = watch::channel(run.shared.clone());
    // Nothing changed: accepted, no switch.
    assert!(
        node_reload(&mut run, &path, &current)
            .await
            .unwrap()
            .is_none()
    );
    // A node-only edit switches the session in place, even when the app-only
    // launch preference changed with it.
    let mut edited = crate::load(&path).unwrap();
    edited.node_mut().port = 58444;
    edited.service.start_automatically = !edited.service.start_automatically;
    crate::save(&path, &edited).unwrap();
    assert!(
        node_reload(&mut run, &path, &current)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(run.shared.config.node().port, 58444);
    // Board edits still need a supervised restart.
    let mut edited = crate::load(&path).unwrap();
    edited.hardware.boards[0].frequency_mhz = 1500.;
    crate::save(&path, &edited).unwrap();
    let error = node_reload(&mut run, &path, &current).await.unwrap_err();
    assert!(format!("{error:#}").contains("Save & Restart"));
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

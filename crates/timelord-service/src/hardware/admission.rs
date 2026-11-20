//! Private offline verification. This runtime has no node task, public listener,
//! history persistence or production metrics, so its proofs cannot be submitted.
use super::*;
use crate::{asic::AsicBackend, runtime::RuntimeState, safety::Qualification};
use anyhow::ensure;
use dg_xch_core::blockchain::{class_group_element::ClassgroupElement, sized_bytes::Bytes32};
use dg_xch_timelord::worker::ProofRequest;
use std::sync::Mutex;
use timelord_core::{Assignment, Chain};

pub(super) fn qualification(
    shared: &Shared,
    board: &BoardConfig,
    engines: usize,
) -> Result<Qualification> {
    let (frequency_mhz, voltage_v) = board.operating_point();
    let admission_frequency = shared
        .runtime
        .lock()
        .expect("runtime")
        .safety
        .clone()
        .zip(board.serial.as_ref())
        .and_then(|(store, serial)| store.board(serial))
        .and_then(|record| record.admission_frequency_mhz)
        .unwrap_or(1450.);
    let point = Point {
        frequency_mhz: if board.profile == ProfileMode::Automatic {
            // Optimization preferences cannot prevent a conservative recovery.
            let minimum = if shared.persistent_owners() && force_safe(shared, board) {
                200.
            } else {
                board.automatic.min_frequency_mhz
            };
            frequency_mhz.min(admission_frequency).max(minimum)
        } else if shared.persistent_owners() && force_safe(shared, board) {
            frequency_mhz.min(admission_frequency).clamp(200., 1450.)
        } else {
            frequency_mhz
        },
        voltage_v,
    };
    Ok(Qualification {
        engines: engines as u8,
        engine_mask: timelord_core::engine_mask(engines)
            .context("Invalid admission engine count")?,
        host: shared
            .native
            .as_ref()
            .map(|f| f.host.clone())
            .unwrap_or_else(automatic::host),
        client_sha256: match &shared.native {
            Some(facts) => facts.hash.clone(),
            None => automatic::fingerprint(&shared.config)?,
        },
        point,
    })
}
#[derive(Clone, Copy)]
pub(super) struct Rules {
    pub timeout: Duration,
    pub cool_observation: Duration,
}
impl Default for Rules {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            cool_observation: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Qualified,
    Cancelled,
}

fn is_typed_failure(error: &anyhow::Error) -> bool {
    error.is::<NativeFailure>() || error.is::<crate::asic::AsicFailure>()
}

fn latched_failure(board_id: &str, fault: Option<crate::board_lifecycle::Fault>) -> anyhow::Error {
    match fault {
        Some(crate::board_lifecycle::Fault::Thermal) => NativeFailure::new(
            NativeFailureKind::Thermal,
            "ASIC admission rejected: temperature_limit",
        )
        .into(),
        Some(crate::board_lifecycle::Fault::InvalidProof) => crate::asic::AsicFailure {
            board_id: board_id.to_owned(),
            verification: true,
            message: "ASIC admission rejected: invalid_proof".into(),
        }
        .into(),
        Some(
            crate::board_lifecycle::Fault::Transport | crate::board_lifecycle::Fault::UncleanOwner,
        )
        | None => NativeFailure::new(
            NativeFailureKind::ControlClosed,
            "ASIC admission rejected: native control closed",
        )
        .into(),
    }
}

pub(super) async fn check(
    shared: &Shared,
    board: &BoardConfig,
    q: &Qualification,
    initialization: tokio::sync::MutexGuard<'static, ()>,
    rules: Rules,
    mut outer_shutdown: watch::Receiver<bool>,
    preparing: Option<timelord_supervisor::progress::Guard>,
) -> Result<Outcome> {
    if *outer_shutdown.borrow() {
        return Ok(Outcome::Cancelled);
    }
    // Do not hold the USB initialization lock or its deadline while waiting for
    // a trial to yield. Pending admission prevents any new exploratory raises.
    drop(preparing);
    drop(initialization);
    let gate = shared.runtime.lock().expect("runtime").experiments.clone();
    let _checking = tokio::select! {
        permit=gate.admission()=>permit,
        _=outer_shutdown.changed()=>return Ok(Outcome::Cancelled),
    };
    let initialization = tokio::select! {
        guard=INITIALIZATION.lock()=>guard,
        _=outer_shutdown.changed()=>return Ok(Outcome::Cancelled),
    };
    let process = shared.process.clone();
    let progress = process.progress.clone();
    let _stage = progress.begin(
        timelord_supervisor::progress::Resource::Admission,
        0,
        360,
        timelord_supervisor::progress::Stage::Admission,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut probe = board.clone();
    probe.client_port = listener.local_addr()?.port();
    probe.profile = ProfileMode::Manual;
    probe.frequency_mhz = q.point.frequency_mhz;
    probe.voltage_v = q.point.voltage_v;
    let mut config = shared.config.clone();
    config.hardware.boards = vec![probe.clone()];
    config.hardware.remembered_boards.clear();
    config.hardware.chain_assignments = None;
    let assignments: Vec<_> = Chain::ALL
        .into_iter()
        .take(usize::from(q.engines))
        .enumerate()
        .map(|(slot, chain)| Assignment {
            board_id: probe.id.clone(),
            chain,
            engine: (0..3)
                .filter(|bit| q.engine_mask & (1 << bit) != 0)
                .nth(slot)
                .expect("qualified engine mask"),
        })
        .collect();
    let mut state = RuntimeState::admission();
    state.boards.insert(
        probe.id.clone(),
        HardwareState {
            lifecycle: {
                let mut owner = crate::board_lifecycle::BoardLifecycle::default();
                owner.bind(probe.serial.as_deref());
                owner.begin_attempt(crate::unix_now());
                owner
            },

            ..Default::default()
        },
    );
    let runtime = Arc::new(Mutex::new(state));
    let (backend, offers) =
        AsicBackend::new(&config, &assignments, runtime.clone(), process.clone())?;
    let backend = Arc::new(backend);
    let private = Arc::new(Shared {
        native: shared.native.clone(),
        config,
        assignments,
        clients: Default::default(),
        process: process.clone(),
        scope: crate::runtime::RuntimeScope::Admission,
        runtime,
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let startup = startup(&private, &probe).await?;
    let (stop, rx) = watch::channel(false);
    let offer = offers
        .get(&probe.id)
        .cloned()
        .context("Missing admission lease pool")?;
    let sockets = tokio::spawn(crate::service::client_listener(
        listener,
        probe.id.clone(),
        q.engines,
        private.clone(),
        rx.clone(),
        Some(offer),
    ));
    if *outer_shutdown.borrow() {
        let _ = stop.send(true);
        sockets.await.context("Admission listener failed")??;
        return Ok(Outcome::Cancelled);
    }
    let child = match launch(&private, &probe, &startup).await {
        Ok(child) => child,
        Err(error) => {
            let _ = stop.send(true);
            let _ = sockets.await;
            return Err(error);
        }
    };
    let (ready, initialized) = oneshot::channel();
    let owner = tokio::spawn(supervise(
        private.clone(),
        probe.clone(),
        child,
        ready,
        rx,
        initialization,
        None,
        startup,
    ));
    let checking = tokio::time::timeout(rules.timeout, async {
        initialized
            .await
            .context("Admission owner did not initialize")?;
        let thermal = shared
            .runtime
            .lock()
            .expect("runtime")
            .safety
            .clone()
            .zip(board.serial.as_ref())
            .and_then(|(store, serial)| store.board(serial))
            .is_some_and(|b| b.quarantine.is_some());
        let cool_limit = if thermal {
            (board.temperature_limit_c - 5.).min(50.)
        } else {
            board.temperature_limit_c - 1.
        };
        let cool = |state: &RuntimeState| {
            state.boards.get(&probe.id).is_some_and(|h| {
                h.lifecycle.status().native_initialized
                    && !h.lifecycle.status().latched_fault
                    && h.temperature_c.is_some_and(|t| t > 0. && t <= cool_limit)
                    && h.temperature_unix <= crate::unix_now()
                    && crate::unix_now() - h.temperature_unix <= 10
            })
        };
        loop {
            {
                let state = private.runtime.lock().expect("runtime");
                let board_state = &state.boards[&probe.id];
                if board_state.lifecycle.status().latched_fault {
                    return Err(latched_failure(
                        &probe.id,
                        board_state.lifecycle.status().fault,
                    ));
                }
                ensure!(
                    board_state.lifecycle.status().phase != crate::board_lifecycle::Phase::Fault,
                    "ASIC admission owner failed"
                );
                if cool(&state) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let first = tokio::time::Instant::now();
        let initial_sample =
            private.runtime.lock().expect("runtime").boards[&probe.id].temperature_samples;
        let mut proofs = JoinSet::new();
        for assignment in &private.assignments {
            let backend = backend.clone();
            let chain = assignment.chain;
            proofs.spawn(async move {
                backend
                    .prove(
                        chain,
                        ProofRequest {
                            generation: 1,
                            challenge: Bytes32::const_new([42; 32]),
                            input: ClassgroupElement::get_default_element(),
                            iterations: 360,
                            discriminant_bits: 1024,
                        },
                    )
                    .await
            });
        }
        while let Some(proof) = proofs.join_next().await {
            proof.context("Admission proof task failed")??;
        }
        tokio::time::sleep_until(first + rules.cool_observation).await;
        loop {
            {
                let state = private.runtime.lock().expect("runtime");
                let board_state = &state.boards[&probe.id];
                if board_state.lifecycle.status().latched_fault {
                    return Err(latched_failure(
                        &probe.id,
                        board_state.lifecycle.status().fault,
                    ));
                }
                ensure!(
                    board_state.lifecycle.status().phase != crate::board_lifecycle::Phase::Fault,
                    "ASIC admission owner failed"
                );
                if cool(&state)
                    && (rules.cool_observation.is_zero()
                        || state.boards[&probe.id].temperature_samples > initial_sample)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<_, anyhow::Error>(())
    });
    let outcome = tokio::select! {
        _=outer_shutdown.changed()=>Ok(Outcome::Cancelled),
        result=checking=>result.context("Offline ASIC admission timed out").and_then(|r|r).map(|()| Outcome::Qualified),
    };
    let _ = stop.send(true);
    backend.stop_all().await;
    let listener_result = sockets
        .await
        .context("Admission listener failed")
        .and_then(|r| r);
    let owner_result = owner
        .await
        .context("Admission native owner failed")
        .and_then(|r| r);
    // Keep the admission watchdog stage until the actual native boundary retires.
    // Never time out an await and leave that owner running untracked.
    if outcome
        .as_ref()
        .err()
        .is_some_and(|error| !is_typed_failure(error))
        && owner_result.as_ref().err().is_some_and(is_typed_failure)
    {
        return Err(owner_result.unwrap_err());
    }
    let outcome = outcome?;
    listener_result?;
    owner_result?;
    ensure!(
        private
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .get(&probe.id)
            .is_some_and(|h| h.lifecycle.status().quiesced),
        "Admission native owner did not confirm hardware quiescence"
    );
    if private.runtime.lock().expect("runtime").failed_proofs != 0 {
        return Err(crate::asic::AsicFailure {
            board_id: board.id.clone(),
            verification: true,
            message: "Admission proof verification failed".into(),
        }
        .into());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::fs::PermissionsExt,
    };

    // Invoked only by a fake native executable. No SDK, USB device or full node.
    #[test]
    fn native_peer_fixture() {
        let Ok(port) = std::env::var("TL_ADMISSION_FIXTURE_PORT") else {
            return;
        };
        let count: usize = std::env::var("TL_ADMISSION_FIXTURE_ENGINES")
            .unwrap()
            .parse()
            .unwrap();
        let invalid = std::env::var("TL_ADMISSION_FIXTURE_INVALID").as_deref() == Ok("1");
        let mut peers = Vec::new();
        for _ in 0..count {
            let port = port.clone();
            peers.push(std::thread::spawn(move || {
                let mut socket = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut header = [0; 4];
                socket.read_exact(&mut header).unwrap();
                assert_eq!(header[0], b'N');
                let size: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
                let mut body = vec![0; size + 101];
                socket.read_exact(&mut body).unwrap();
                socket.write_all(b"OK").unwrap();
                let mut length = [0; 2];
                socket.read_exact(&mut length).unwrap();
                let size: usize = std::str::from_utf8(&length).unwrap().parse().unwrap();
                let mut request = vec![0; size];
                socket.read_exact(&mut request).unwrap();
                assert_eq!(std::str::from_utf8(&request).unwrap(), "360");
                if let Ok(path) = std::env::var("TL_ADMISSION_READY_FILE") {
                    std::fs::write(path, b"proof requested").unwrap();
                }
                let blob = include_bytes!("../../tests/fixtures/checkpointed-proof.bin");
                let mut raw = 360_u64.to_be_bytes().to_vec();
                raw.extend(100_u64.to_be_bytes());
                raw.extend_from_slice(&blob[..100]);
                raw.push(((blob.len() - 200) / 141) as u8);
                raw.extend_from_slice(&blob[100..]);
                if invalid {
                    raw[16] ^= 1;
                }
                let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
                socket.write_all(&(hex.len() as u32).to_be_bytes()).unwrap();
                socket.write_all(hex.as_bytes()).unwrap();
                let mut drain = [0; 1024];
                while socket.read(&mut drain).is_ok_and(|n| n > 0) {}
            }));
        }
        for peer in peers {
            peer.join().unwrap();
        }
    }
    #[tokio::test]
    async fn admission_verifies_all_engines_without_publishing_and_requires_confirmed_shutdown() {
        let dir = std::env::temp_dir().join(format!("tl-admission-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        for (variant, invalid, unconfirmed, temperature) in [
            ("good", false, false, 40),
            ("bad-proof", true, false, 40),
            ("unclean", false, true, 40),
            ("stale-temperature", false, false, 40),
            ("hot", false, false, 100),
            ("cancelled", false, false, 40),
            ("cancelled-unclean", false, true, 40),
            ("cancelled-handshake", false, false, 40),
            ("cancelled-handshake-unclean", false, true, 40),
        ] {
            let script = dir.join(variant);
            let cancelling = variant.starts_with("cancelled");
            let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            std::fs::write(&script,format!(r#"#!/bin/sh
export TL_ADMISSION_FIXTURE_PORT="$7" TL_ADMISSION_FIXTURE_ENGINES="$8" TL_ADMISSION_FIXTURE_INVALID={invalid} TL_ADMISSION_READY_FILE={started}
{exe} --exact hardware::admission::tests::native_peer_fixture --nocapture >/dev/null 2>/dev/null &
pid=$!
trap 'kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
trap 'exit 143' TERM
printf '%s\n' 'Connected to timelord, waiting for challenge' 'ASIC Temp = {temperature} C; Frequency = 1450 MHz' >&2
while read command version id frequency voltage; do
 if [ {handshake} = 1 ]; then
  printf '%s\n' 'handshake requested' > {started}
  continue
 fi
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
{marker}
exit 0
"#,handshake=if variant.contains("handshake"){1}else{0},invalid=if invalid{1}else{0},temperature=temperature,exe=quote(exe.to_str().unwrap()),started=quote(script.with_extension("started").to_str().unwrap()),marker=if unconfirmed{""}else{"printf '%s\\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2"})).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut config = Config::default();
            let board:BoardConfig=toml::from_str("id='probe'\nname='Probe'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'\nfrequency_mhz=1450.0\nvoltage_v=1.0").unwrap();
            config.hardware.client_binary = Some(script.clone());
            config.hardware.boards = vec![board.clone()];
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
                config,
                assignments: vec![],
                clients: Default::default(),
                process: crate::operational::State::fixture(),
                scope: crate::runtime::RuntimeScope::Production,
                runtime: Default::default(),
                started_unix: crate::unix_now(),
                service_started_unix: crate::unix_now(),
            };
            let q = qualification(&shared, &board, 3).unwrap();
            let process = shared.process.clone();
            let metrics = process.metrics.clone();
            let epoch = metrics.begin_session();
            metrics.verified(epoch, Chain::Challenge, 1234.);
            let io = process.progress.begin(
                timelord_supervisor::progress::Resource::ProofIo(
                    timelord_supervisor::progress::ProofLane::Challenge,
                ),
                777,
                100,
                timelord_supervisor::progress::Stage::NativeWork,
            );
            let arithmetic = process.progress.begin(
                timelord_supervisor::progress::Resource::Arithmetic(
                    timelord_supervisor::progress::ProofLane::Reward,
                ),
                778,
                100,
                timelord_supervisor::progress::Stage::Verifying,
            );
            let original_ids: Vec<_> = process
                .progress
                .snapshot(0, 0)
                .unwrap()
                .work
                .iter()
                .flatten()
                .map(|w| w.id)
                .collect();
            let before = metrics.render(&shared, 12345).unwrap();
            let (stop, rx) = watch::channel(false);
            let checking = check(
                &shared,
                &board,
                &q,
                INITIALIZATION.lock().await,
                Rules {
                    timeout: if variant == "stale-temperature" {
                        Duration::from_millis(500)
                    } else {
                        Duration::from_secs(8)
                    },
                    cool_observation: if cancelling {
                        Duration::from_secs(30)
                    } else if variant == "stale-temperature" {
                        Duration::from_millis(10)
                    } else {
                        Duration::ZERO
                    },
                },
                rx,
                None,
            );
            tokio::pin!(checking);
            let result = if cancelling {
                tokio::select! {
                    result = &mut checking => panic!("admission exited before cancellation: {result:?}"),
                    _ = async {
                        while !script.with_extension("started").exists() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    } => {
                        stop.send(true).unwrap();
                        checking.await
                    }
                }
            } else {
                checking.await
            };
            assert_eq!(
                metrics.render(&shared, 12345).unwrap(),
                before,
                "admission {variant} published production telemetry"
            );
            let after_ids: Vec<_> = process
                .progress
                .snapshot(0, 0)
                .unwrap()
                .work
                .iter()
                .flatten()
                .map(|w| w.id)
                .collect();
            assert_eq!(
                after_ids, original_ids,
                "admission {variant} replaced production work"
            );
            drop((io, arithmetic));
            if cancelling && !unconfirmed {
                assert_eq!(result.unwrap(), Outcome::Cancelled);
                continue;
            }
            assert_eq!(result.is_ok(), variant == "good", "{variant}: {result:?}");
            if invalid {
                assert!(
                    result
                        .as_ref()
                        .unwrap_err()
                        .downcast_ref::<crate::asic::AsicFailure>()
                        .is_some_and(|e| e.verification)
                );
            }
            if variant == "hot" {
                let error = result.as_ref().unwrap_err();
                assert_eq!(classify(error), crate::board_lifecycle::Fault::Thermal);
                let failure = error.downcast_ref::<NativeFailure>().unwrap();
                assert_eq!(failure.kind, NativeFailureKind::Thermal);
                assert_eq!(
                    failure.message,
                    "ASIC admission rejected: temperature_limit"
                );
                assert_eq!(
                    crate::observability::restart_reason(error),
                    crate::observability::RestartReason::Thermal
                );
            }
            let state = shared.runtime.lock().unwrap();
            assert_eq!(state.proofs_delivered, 0);
            assert_eq!(state.failed_proofs, 0);
            assert!(state.observations.is_empty() && state.estimated_observations.is_empty());
            assert!(shared.metrics().is_some());
            assert!(shared.clients.lock().unwrap().is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn admission_native_events_do_not_publish_production_recovery_or_thermal_metrics() {
        let board: BoardConfig =
            toml::from_str("id='probe'\nname='Probe'\nclient_port=18110\nprofile='manual'")
                .unwrap();
        let mut config = Config::default();
        config.hardware.boards = vec![board.clone()];
        let process = crate::operational::State::fixture();
        let public = Shared {
            native: None,
            config: config.clone(),
            assignments: vec![],
            clients: Default::default(),
            process: process.clone(),
            scope: crate::runtime::RuntimeScope::Production,
            runtime: Arc::new(Mutex::new(RuntimeState::new(false))),
            started_unix: 1,
            service_started_unix: 1,
        };
        let private = Shared {
            native: None,
            config,
            assignments: vec![],
            clients: Default::default(),
            process: process.clone(),
            scope: crate::runtime::RuntimeScope::Admission,
            runtime: Arc::new(Mutex::new(RuntimeState::admission())),
            started_unix: 1,
            service_started_unix: 1,
        };
        let before = process.metrics.render(&public, 12345).unwrap();
        let mut child = Command::new("/bin/sh").args(["-c", "printf '%s\\n' 'VDF 1: Restarting VDF at 123 iters' 'ASIC Temp = 100 C; Frequency = 1450 MHz' >&2"])
            .stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let (ready, _rx) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        assert_eq!(log.read(&private, &board).await.unwrap().unwrap().engine, 1);
        let error = log.read(&private, &board).await.unwrap_err();
        assert_eq!(classify(&error), crate::board_lifecycle::Fault::Thermal);
        child.wait().await.unwrap();
        assert_eq!(
            private.runtime.lock().unwrap().boards[&board.id].recoveries,
            1
        );
        assert_eq!(process.metrics.render(&public, 12345).unwrap(), before);
    }
}

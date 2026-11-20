use super::*;

#[test]
fn timelord_handshake_capability_is_advertised_once() {
    let greeting = timelord_handshake("mainnet");
    assert_eq!(
        greeting
            .capabilities
            .iter()
            .filter(|capability| capability.0 == 6)
            .count(),
        1
    );
    assert!(greeting.capabilities.contains(&(6, "1".to_owned())));
    let other = handshake("mainnet", NodeType::FullNode, 0);
    assert!(
        !other
            .capabilities
            .iter()
            .any(|capability| capability.0 == 6)
    );
}

use crate::unix_now;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use std::{fs, path::PathBuf};
use tokio::net::TcpListener;
struct Files(PathBuf);
#[test]
fn busy_node_confirmation_keeps_the_connection_and_withholds_stale_work() {
    let sent = Instant::now();
    for seconds in [5, 10, 25, 32, 59] {
        assert!(
            waiting_for_infusion_confirmation(
                true,
                Some(sent),
                sent + Duration::from_secs(seconds),
            )
            .unwrap(),
            "Expired scheduler must remain blocked while node validation finishes"
        );
    }
    for seconds in [60, 90] {
        assert!(
            waiting_for_infusion_confirmation(
                true,
                Some(sent),
                sent + Duration::from_secs(seconds),
            )
            .is_err(),
            "Repeated node messages must not extend the submission deadline"
        );
    }
    assert!(waiting_for_infusion_confirmation(true, None, sent).is_err());
    // An accepted peak clears the scheduler's wait and permits fresh work.
    assert!(!waiting_for_infusion_confirmation(false, None, sent).unwrap());
    assert!(
        !waiting_for_infusion_confirmation(false, Some(sent), sent + Duration::from_secs(90))
            .unwrap()
    );
    // A later successful infusion submission gets its own bounded window.
    let later = sent + Duration::from_secs(100);
    assert!(
        waiting_for_infusion_confirmation(true, Some(later), later + Duration::from_secs(5))
            .unwrap()
    );
}

#[test]
fn current_unfinished_envelope_is_exact_and_does_not_silently_ignore_mmr() {
    let current = include_bytes!("../../tests/fixtures/unfinished-block.bin");
    let decoded = decode_unfinished(current).unwrap();
    assert_eq!(decoded.difficulty, 2416);
    assert_eq!(decoded.sub_slot_iters, 587202560);
    let legacy = &current[..current.len() - 1];
    assert_eq!(decode_unfinished(legacy).unwrap(), decoded);
    assert!(decode_unfinished(&legacy[..legacy.len() - 1]).is_err());
    let mut mmr = legacy.to_vec();
    mmr.push(1);
    mmr.extend([42; 32]);
    assert_eq!(
        decode_unfinished(&mmr).unwrap().header_mmr_root,
        Some(dg_xch_core::blockchain::sized_bytes::Bytes32::const_new(
            [42; 32]
        ))
    );
    for extension in [vec![2], vec![0, 0], vec![1], vec![1; 32], vec![1; 34]] {
        let mut malformed = legacy.to_vec();
        malformed.extend(extension);
        assert!(decode_unfinished(&malformed).is_err());
    }
}
#[test]
fn invalid_candidate_does_not_reset_the_scheduler_or_discard_later_work() {
    use dg_xch_core::protocols::timelord::{NewPeakTimelord, NewUnfinishedBlockTimelord};
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../integrations/dg-xch/vendor/timelord/tests/fixtures/chia_hf2.json"
    ))
    .unwrap();
    let node = crate::network::tests::node();
    let mut constants = crate::network::resolve(&node).unwrap().constants;
    constants.hard_fork_height = 0;
    constants.epoch_blocks = 768;
    constants.number_zero_bits_plot_filter_v2 = 7;
    constants.max_plot_strength = 9;
    let mut scheduler = dg_xch_timelord::scheduler::Scheduler::new(constants, false, None).unwrap();
    let metrics = crate::observability::Metrics::new(timelord_core::Network::Mainnet).unwrap();
    let version = ChiaProtocolVersion::default();
    let decode_hex = |text: &str| {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>()
    };
    let sequence = fixture["peer_sequence"].as_array().unwrap();
    let raw = decode_hex(sequence[0]["bytes"].as_str().unwrap());
    let peak: NewPeakTimelord = decode_exact(&raw).unwrap();
    scheduler.set_peak(peak).unwrap();
    let generation = scheduler.generation();
    let raw = decode_hex(sequence[1]["bytes"].as_str().unwrap());
    let mut bad: NewUnfinishedBlockTimelord = decode_exact(&raw).unwrap();
    bad.reward_chain_block.proof_of_space =
        serde_json::from_value(fixture["rows"][0]["reward_block"]["proof_of_space"].clone())
            .unwrap();
    bad.reward_chain_block.proof_of_space.proof =
        serde_json::from_value(serde_json::json!("0x00")).unwrap();
    let rejected = handle_unfinished(&mut scheduler, &bad.to_bytes(version).unwrap(), &metrics);
    assert!(
        rejected.is_err(),
        "invalid candidate must be rejected, not deferred"
    );
    assert!(scheduler.initialized());
    assert_eq!(scheduler.generation(), generation);
    let expected_hash = decode_exact::<NewUnfinishedBlockTimelord>(&raw)
        .unwrap()
        .reward_chain_block
        .hash()
        .unwrap();
    let admitted = handle_unfinished(&mut scheduler, &raw, &metrics).unwrap();
    assert_eq!(admitted, Admission::Admitted(expected_hash));
    assert!(scheduler.initialized());
    assert_eq!(scheduler.generation(), generation);
    assert!(
        !scheduler
            .add_unfinished(decode_exact(&raw).unwrap())
            .unwrap(),
        "later valid candidate was already admitted"
    );
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn early_infused_request_keeps_the_validated_input_and_generation() {
    use dg_xch_core::blockchain::{
        class_group_element::ClassgroupElement,
        sized_bytes::{Bytes32, Bytes100},
    };
    let input = ClassgroupElement {
        data: Bytes100::const_new([5; 100]),
    };
    let work = primer_request(Bytes32::const_new([42; 32]), input, 73, 1024);
    assert_eq!(work.input, input);
    assert_eq!(work.generation, 73);
    assert_eq!(work.iterations, 1);
    assert_eq!(work.discriminant_bits, 1024);
}
#[test]
fn peer_role_network_and_message_type_are_all_required() {
    let mut greeting = handshake("mainnet", NodeType::FullNode, 8444);
    assert!(validate_handshake(ProtocolMessageTypes::Handshake, &greeting, "mainnet").is_ok());
    assert!(validate_handshake(ProtocolMessageTypes::Handshake, &greeting, "testnet11").is_err());
    greeting.node_type = NodeType::Farmer as u8;
    assert!(validate_handshake(ProtocolMessageTypes::Handshake, &greeting, "mainnet").is_err());
    assert!(
        validate_handshake(ProtocolMessageTypes::NewPeakTimelord, &greeting, "mainnet").is_err()
    );
}
#[tokio::test]
async fn custom_network_uses_the_exact_exported_id_over_authenticated_tls() {
    fixture("chia.net", false, true).await.unwrap();
}
async fn fixture(server_name: &str, board_fault: bool, custom: bool) -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut parameters = CertificateParams::new(vec!["fixture-ca".into()])?;
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate()?;
    let ca = parameters.self_signed(&ca_key)?;
    let server_key = KeyPair::generate()?;
    let server =
        CertificateParams::new(vec!["chia.net".into()])?.signed_by(&server_key, &ca, &ca_key)?;
    let client_key = KeyPair::generate()?;
    let client = CertificateParams::new(vec!["timelord-fixture".into()])?.signed_by(
        &client_key,
        &ca,
        &ca_key,
    )?;
    let directory = std::env::temp_dir().join(format!(
        "timelord-tls-fixture-{}-{}-{}",
        std::process::id(),
        server_name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir(&directory)?;
    let _cleanup = Files(directory.clone());
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    fs::write(directory.join("client.crt"), client.pem())?;
    fs::write(directory.join("client.key"), client_key.serialize_pem())?;
    fs::set_permissions(
        directory.join("client.key"),
        fs::Permissions::from_mode(0o600),
    )?;
    fs::write(directory.join("ca.crt"), ca.pem())?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone())?;
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let server_tls = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(server_key.serialize_der())
                .map_err(anyhow::Error::msg)?,
        )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut config = Config::default();
    config.node.host = "127.0.0.1".into();
    config.node.port = listener.local_addr()?.port();
    config.node.server_name = Some(server_name.into());
    let network_id = if custom {
        "ephemeral-fixture"
    } else {
        "testnet11"
    };
    if custom {
        config.node.network = crate::network::tests::node().network;
        config.node.network_definition = crate::network::tests::node().network_definition;
    } else {
        config.node.network = timelord_core::Network::Testnet11;
    }
    config.node.certificate = Some(directory.join("client.crt"));
    config.node.private_key = Some(directory.join("client.key"));
    config.node.ca_certificate = Some(directory.join("ca.crt"));
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await?;
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(server_tls))
            .accept(tcp)
            .await?;
        let mut socket = tokio_tungstenite::accept_async(tls).await?;
        let incoming = socket.next().await.context("handshake missing")??;
        let Message::Binary(bytes) = incoming else {
            bail!("bad frame");
        };
        let message: ChiaMessage = decode_exact(&bytes)?;
        let greeting: Handshake = decode_exact(message.data.as_slice())?;
        assert_eq!(greeting.node_type, NodeType::Timelord as u8);
        assert_eq!(greeting.network_id, network_id);
        socket
            .send(encode(
                ProtocolMessageTypes::Handshake,
                &handshake(network_id, NodeType::FullNode, 8444),
                None,
            )?)
            .await?;
        if board_fault {
            socket
                .send(Message::Ping(b"board-isolation".to_vec().into()))
                .await?;
            loop {
                match socket
                    .next()
                    .await
                    .context("Peer closed during board fault")??
                {
                    Message::Pong(value) if value.as_ref() == b"board-isolation" => break,
                    Message::Ping(value) => socket.send(Message::Pong(value)).await?,
                    _ => {}
                }
            }
        } else {
            let _ = socket.next().await;
        }
        Ok::<_, anyhow::Error>(())
    });
    if board_fault {
        config.hardware.boards = vec![toml::from_str(
            "id='failed'\nname='Failed'\nclient_port=18110",
        )?];
        let process = crate::operational::State::fixture();
        let runtime = Arc::new(std::sync::Mutex::new(RuntimeState::default()));
        runtime.lock().unwrap().boards.insert(
            "failed".into(),
            HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                    crate::board_lifecycle::Phase::Fault,
                    false,
                    Some(crate::board_lifecycle::Fault::InvalidProof),
                    None,
                    0,
                ),
                ..Default::default()
            },
        );
        let (backend, _offers) = AsicBackend::new(
            &config,
            &config.assignments().map_err(anyhow::Error::msg)?,
            runtime.clone(),
            process.clone(),
        )?;
        let shared = Arc::new(Shared {
            native: None,
            assignments: config.assignments().map_err(anyhow::Error::msg)?,
            config,
            clients: Default::default(),
            process,
            scope: crate::runtime::RuntimeScope::Production,
            runtime,
            started_unix: unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let session = tokio::spawn(session(shared, Arc::new(backend)));
        let result = tokio::time::timeout(Duration::from_secs(3), task).await;
        session.abort();
        let _ = session.await;
        result???;
        return Ok(());
    }
    let result = connect(&config).await;
    match result {
        Ok(mut socket) => {
            socket.close(None).await?;
            task.await??;
            Ok(())
        }
        Err(error) => {
            task.abort();
            let _ = task.await;
            Err(error)
        }
    }
}
#[tokio::test]
async fn submissions_count_only_successful_sends() {
    let metrics = crate::observability::Metrics::new(timelord_core::Network::Mainnet).unwrap();
    let process = crate::operational::State::new(metrics.clone());
    let shared = Shared {
        native: None,
        config: Config::default(),
        assignments: vec![],
        clients: Default::default(),
        process,
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Arc::new(std::sync::Mutex::new(RuntimeState::new(false))),
        started_unix: unix_now(),
        service_started_unix: unix_now(),
    };
    let stage = shared.process.progress.begin(
        timelord_supervisor::progress::Resource::Scheduler,
        0,
        0,
        timelord_supervisor::progress::Stage::Scheduler,
    );
    assert!(
        submit(&shared, SubmissionKind::Signage, &stage, async {
            bail!("fixture send failed")
        })
        .await
        .is_err()
    );
    assert_eq!(metrics.counter("submissions_total", &["signage"]), 0);
    submit(&shared, SubmissionKind::Signage, &stage, async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(metrics.counter("submissions_total", &["signage"]), 1);
}
#[tokio::test]
async fn submission_transitions_the_scheduler_owner_instead_of_replacing_it() {
    use timelord_supervisor::progress::{Resource, Stage};
    let metrics = crate::observability::Metrics::new(timelord_core::Network::Mainnet).unwrap();
    let process = crate::operational::State::new(metrics.clone());
    let shared = Shared {
        native: None,
        config: Config::default(),
        assignments: vec![],
        clients: Default::default(),
        process,
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Arc::new(std::sync::Mutex::new(RuntimeState::new(false))),
        started_unix: unix_now(),
        service_started_unix: unix_now(),
    };
    let process = shared.process.clone();
    let guard = process
        .progress
        .begin(Resource::Scheduler, 12, 42, Stage::Scheduler);
    let scheduler = |sequence| {
        process
            .progress
            .snapshot(1, sequence)
            .unwrap()
            .slot(Resource::Scheduler)
    };
    let previous = scheduler(1).unwrap();
    submit(&shared, SubmissionKind::Signage, &guard, async {
        let work = scheduler(2).unwrap();
        assert_eq!(work.id, previous.id);
        assert_eq!(work.generation, previous.generation);
        assert_eq!(work.stage, Stage::Submitting);
        Ok(())
    })
    .await
    .unwrap();
    assert!(scheduler(3).is_some());
    drop(guard);
    assert!(scheduler(4).is_none());
}

#[tokio::test]
async fn authenticates_a_mutual_tls_chia_websocket_with_a_fixture_only_identity() {
    fixture("chia.net", false, false).await.unwrap();
}
#[tokio::test]
async fn rejects_an_incorrect_tls_server_name() {
    assert!(fixture("wrong.invalid", false, false).await.is_err());
}
#[tokio::test]
async fn full_node_session_keeps_answering_while_a_board_is_latched_off() {
    fixture("chia.net", true, false).await.unwrap();
}

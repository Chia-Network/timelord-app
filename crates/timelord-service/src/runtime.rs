//! Native coordinator: upstream scheduler/transport, persistent existing ASIC clients.
use crate::{
    Shared,
    asic::{AsicBackend, AsicFailure, ProofTiming},
    unix_now,
};
use anyhow::{Context, Result, bail};
use dg_xch_core::{
    consensus::chain_definition::ChainSelection,
    protocols::{ChiaMessage, NodeType, ProtocolMessageTypes, shared::Handshake},
};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use dg_xch_servers::transport::{
    SERVICE_MESSAGE_LIMIT, TlsIdentity, decode_exact, encode, handshake,
};
use dg_xch_timelord::scheduler::{Output, Scheduler, WorkPlan, WorkResult};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use timelord_core::{Chain, Config};
use tokio::{net::TcpStream, sync::watch, task::JoinSet};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub type Socket = WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;
#[derive(Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: u64,
    pub chain: Chain,
    pub timestamp_unix: u64,
    pub work_started_unix: u64,
    pub ips: f64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub timestamp_unix: u64,
    pub board_id: Option<String>,
    pub kind: String,
    pub end_unix: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawState {
    Running,
    Sampling,
    Idle,
}
#[derive(Clone, Serialize)]
pub struct RawSpeed {
    pub state: RawState,
    pub engine: usize,
    pub ips: f64,
    pub timestamp_unix: u64,
}
#[derive(Clone, Default, Serialize)]
pub struct HardwareState {
    #[serde(skip)]
    pub generation: u64,
    pub device_location: Option<u32>,
    #[serde(skip)]
    pub started_unix: u64,
    #[serde(skip)]
    pub latched_fault: bool,
    // Set only by discovery while this board's native owner is closed.
    #[serde(skip)]
    pub usb_present: Option<bool>,
    #[serde(skip)]
    pub fault_reason: Option<String>,
    pub raw_speeds: Vec<RawSpeed>,
    pub temperature_c: Option<f64>,
    pub frequency_mhz: Option<f64>,
    pub voltage_v: Option<f64>,
    pub state: String,
    pub recoveries: u64,
    pub temperature_unix: u64,
    pub automatic: Option<crate::automatic::Status>,
}
impl HardwareState {
    pub fn raw_sample(&mut self, engine: usize, ips: f64, state: RawState, now: u64) {
        if let Some(sample) = self.raw_speeds.iter_mut().find(|s| s.engine == engine) {
            sample.state = state;
            if state == RawState::Running {
                sample.ips = ips;
                sample.timestamp_unix = now;
            }
        } else {
            self.raw_speeds.push(RawSpeed {
                engine,
                ips,
                state,
                timestamp_unix: now,
            });
        }
    }
}
pub struct RuntimeState {
    pub phase: String,
    pub node: String,
    pub message: String,
    pub observations: Vec<Observation>,
    pub estimated_observations: Vec<Observation>,
    pub events: Vec<Event>,
    pub proofs_delivered: u64,
    pub failed_proofs: u64,
    pub tuning_paused: bool,
    pub(crate) events_created: u64,
    pub(crate) observations_created: u64,
    pub(crate) archive: Option<crate::history_store::Snapshot>,
    pub boards: std::collections::HashMap<String, HardwareState>,
}
impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            phase: "setup_required".into(),
            node: "not_configured".into(),
            message: "Connect a full node to get started.".into(),
            observations: vec![],
            estimated_observations: vec![],
            events: vec![],
            proofs_delivered: 0,
            failed_proofs: 0,
            tuning_paused: false,
            events_created: 0,
            observations_created: 0,
            archive: None,
            boards: Default::default(),
        }
    }
}
impl RuntimeState {
    pub fn with_tuning_paused(tuning_paused: bool) -> Self {
        Self {
            tuning_paused,
            ..Default::default()
        }
    }
    pub fn transition(&mut self, phase: &str, node: &str, message: &str) {
        self.phase = phase.into();
        self.node = node.into();
        self.message = message.into();
    }
    pub fn event(&mut self, board_id: Option<String>, kind: &str) {
        self.events_created += 1;
        self.events.push(Event {
            id: self.events_created,
            timestamp_unix: unix_now(),
            board_id,
            kind: kind.into(),
            end_unix: None,
        });
        self.events
            .retain(|e| e.timestamp_unix >= unix_now().saturating_sub(86400));
    }
    fn connected(&mut self) {
        for event in &mut self.events {
            if event.kind == "node_unavailable" && event.end_unix.is_none() {
                event.end_unix = Some(unix_now());
            }
        }
    }
    pub fn estimated(&mut self, chain: Chain, ips: f64, work_started_unix: u64) {
        self.observations_created += 1;
        self.estimated_observations.push(Observation {
            id: self.observations_created,
            chain,
            ips,
            timestamp_unix: unix_now(),
            work_started_unix,
        });
        self.estimated_observations
            .retain(|o| o.timestamp_unix >= unix_now().saturating_sub(86400));
        let excess = self.estimated_observations.len().saturating_sub(86400);
        self.estimated_observations.drain(..excess);
    }
    fn delivered(&mut self, speeds: Vec<(Chain, ProofTiming)>) {
        for (chain, timing) in speeds {
            let ips = timing.delivered_ips();
            self.observations_created += 1;
            self.observations.push(Observation {
                id: self.observations_created,
                chain,
                ips,
                timestamp_unix: unix_now(),
                work_started_unix: timing.started_unix(),
            });
            self.proofs_delivered += 1;
        }
        if self.observations.len() > 86400 {
            self.observations.drain(..self.observations.len() - 86400);
        }
        self.observations
            .retain(|e| e.timestamp_unix >= unix_now().saturating_sub(86400));
        self.phase = "running".into();
        self.message = "Producing verified proofs for your full node.".into();
    }
}
pub fn chain(config: &Config) -> Result<ChainSelection> {
    let name = match config.node.network {
        timelord_core::Network::Mainnet => "mainnet",
        timelord_core::Network::Testnet11 => "testnet11",
    };
    ChainSelection::from_config(name, None).map_err(anyhow::Error::msg)
}
pub async fn connect(config: &Config) -> Result<Socket> {
    crate::connection::validate_identity(&config.node)?;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let identity = TlsIdentity {
        certificate: config
            .node
            .certificate
            .clone()
            .context("Choose a client certificate")?,
        private_key: config
            .node
            .private_key
            .clone()
            .context("Choose a client private key")?,
        ca_certificate: config
            .node
            .ca_certificate
            .clone()
            .context("Choose the node CA certificate")?,
    };
    let tls = identity.client().context("Loading node credentials")?;
    let network = chain(config)?
        .handshake_network_id()
        .map_err(anyhow::Error::msg)?;
    let server_name = config
        .node
        .server_name
        .clone()
        .unwrap_or_else(|| "chia.net".into());
    let mut socket = tokio::time::timeout(Duration::from_secs(15), async {
        let tcp = TcpStream::connect((config.node.host.as_str(), config.node.port)).await?;
        let tls = TlsConnector::from(tls)
            .connect(
                rustls::pki_types::ServerName::try_from(server_name.clone())?,
                tcp,
            )
            .await?;
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            format!("wss://{server_name}:{}/ws", config.node.port),
            tls,
            Some(
                WebSocketConfig::default()
                    .max_message_size(Some(SERVICE_MESSAGE_LIMIT))
                    .max_frame_size(Some(SERVICE_MESSAGE_LIMIT)),
            ),
        )
        .await?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .context("Node connection timed out")??;
    send(
        &mut socket,
        encode(
            ProtocolMessageTypes::Handshake,
            &handshake(&network, NodeType::Timelord, 0),
            None,
        )?,
    )
    .await?;
    let incoming = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .context("Node handshake timed out")?
        .context("Node closed during handshake")??;
    let Message::Binary(bytes) = incoming else {
        bail!("The endpoint did not return a Chia handshake");
    };
    let message: ChiaMessage = decode_exact(&bytes)?;
    let greeting: Handshake = decode_exact(message.data.as_slice())?;
    validate_handshake(message.msg_type, &greeting, &network)?;
    Ok(socket)
}
pub fn validate_handshake(
    kind: ProtocolMessageTypes,
    greeting: &Handshake,
    network: &str,
) -> Result<()> {
    if kind != ProtocolMessageTypes::Handshake
        || greeting.node_type != NodeType::FullNode as u8
        || greeting.network_id != network
    {
        bail!("The endpoint is not a full node on the selected network");
    }
    Ok(())
}
// Current Chia appends an optional header MMR root to the legacy unfinished
// block envelope. The pinned scheduler supports pre-MMR consensus only. Accept
// the absent root, require exact framing, and fail closed on an active MMR root.
fn decode_unfinished(
    bytes: &[u8],
) -> Result<dg_xch_core::protocols::timelord::NewUnfinishedBlockTimelord> {
    use dg_xch_core::protocols::timelord::NewUnfinishedBlockTimelord;
    if bytes.len() > SERVICE_MESSAGE_LIMIT {
        bail!("Unfinished block exceeds message limit");
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let value =
        NewUnfinishedBlockTimelord::from_bytes(&mut cursor, ChiaProtocolVersion::default())?;
    match &bytes[cursor.position() as usize..] {
        [] | [0] => Ok(value),
        [1, root @ ..] if root.len() == 32 => {
            bail!("The pinned scheduler does not support header MMR roots yet")
        }
        _ => bail!("Malformed unfinished-block extension"),
    }
}
async fn send(socket: &mut Socket, message: Message) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), socket.send(message))
        .await
        .context("Node send timed out")??;
    Ok(())
}
fn proof_digest(
    proof: &dg_xch_core::blockchain::vdf_proof::VdfProof,
) -> Result<dg_xch_core::blockchain::sized_bytes::Bytes32> {
    Ok(dg_xch_core::blockchain::sized_bytes::Bytes32::const_new(
        dg_xch_core::utils::hash_256(proof.to_bytes(ChiaProtocolVersion::default())?),
    ))
}
// The upstream CPU-oriented next_plan omits ICC on signage points. Prime the
// validated ICC input separately so a physical engine keeps squaring before its
// first infusion/EOS proof is requested. Primer proofs are never counted or sent.
fn primer_request(
    challenge: dg_xch_core::blockchain::sized_bytes::Bytes32,
    input: dg_xch_core::blockchain::class_group_element::ClassgroupElement,
    generation: u64,
    bits: usize,
) -> dg_xch_timelord::worker::ProofRequest {
    dg_xch_timelord::worker::ProofRequest {
        generation,
        challenge,
        input,
        iterations: 1,
        discriminant_bits: bits,
    }
}
// Input translation follows Scheduler.set_peak at the pinned upstream revision.
fn infused_from_peak(
    peak: &dg_xch_core::protocols::timelord::NewPeakTimelord,
    generation: u64,
    bits: usize,
    minimum_deficit: u8,
) -> Result<Option<dg_xch_timelord::worker::ProofRequest>> {
    use dg_xch_core::blockchain::{
        challenge_block_info::ChallengeBlockInfo, class_group_element::ClassgroupElement,
    };
    let block = &peak.reward_chain_block;
    let input = if let Some(info) = block.infused_challenge_chain_ip_vdf {
        Some((info.challenge, info.output))
    } else if peak.deficit == minimum_deficit - 1 {
        Some((
            ChallengeBlockInfo {
                proof_of_space: block.proof_of_space.clone(),
                challenge_chain_sp_vdf: block.challenge_chain_sp_vdf,
                challenge_chain_sp_signature: block.challenge_chain_sp_signature,
                challenge_chain_ip_vdf: block.challenge_chain_ip_vdf,
            }
            .hash()?,
            ClassgroupElement::get_default_element(),
        ))
    } else {
        None
    };
    Ok(input.map(|(challenge, input)| primer_request(challenge, input, generation, bits)))
}
fn prime_infused(
    primers: &mut JoinSet<Result<()>>,
    backend: Arc<AsicBackend>,
    work: Option<dg_xch_timelord::worker::ProofRequest>,
) {
    primers.abort_all();
    primers.spawn(async move {
        if let Some(work) = work {
            backend.prime(work).await?;
        } else {
            backend.stop_chain(Chain::InfusedChallenge).await;
        }
        Ok(())
    });
}
struct Completed {
    plan: WorkPlan,
    result: WorkResult,
    speeds: Vec<(Chain, ProofTiming)>,
}
async fn execute(backend: Arc<AsicBackend>, plan: WorkPlan) -> Result<Completed> {
    let cc = backend.prove_when_available(Chain::Challenge, plan.challenge.clone(), true);
    let rc = backend.prove_when_available(Chain::Reward, plan.reward.clone(), true);
    let icc = async {
        match &plan.infused {
            Some(work) => backend
                .prove_when_available(Chain::InfusedChallenge, work.clone(), true)
                .await
                .map(Some),
            None => Ok(None),
        }
    };
    let (cc, rc, icc) = tokio::try_join!(cc, rc, icc)?;
    let mut speeds = vec![(Chain::Challenge, cc.1), (Chain::Reward, rc.1)];
    if let Some((_, speed)) = &icc {
        speeds.push((Chain::InfusedChallenge, *speed));
    }
    let bundle_ready = Instant::now();
    for (chain, timing) in &speeds {
        tracing::info!(
            ?chain,
            generation = plan.generation,
            total_iters = plan.total_iters,
            ready_unix_ms = timing.ready_unix_ms,
            bundle_wait_ms = bundle_ready
                .saturating_duration_since(timing.ready_at)
                .as_secs_f64()
                * 1000.,
            "Verified chain proof ready for bundle"
        );
    }
    Ok(Completed {
        plan,
        result: WorkResult {
            challenge: cc.0,
            reward: rc.0,
            infused: icc.map(|(proof, _)| proof),
        },
        speeds,
    })
}
// The upstream scheduler asks for resynchronization five seconds after an
// infusion. Give node validation time to finish without disconnecting or asking
// the expired scheduler for more work based on the old peak.
const INFUSION_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(60);
fn waiting_for_infusion_confirmation(
    scheduler_expired: bool,
    submitted_at: Option<Instant>,
    now: Instant,
) -> Result<bool> {
    if !scheduler_expired {
        return Ok(false);
    }
    if submitted_at
        .is_none_or(|sent| now.saturating_duration_since(sent) >= INFUSION_CONFIRMATION_TIMEOUT)
    {
        bail!(
            "Node has not confirmed the infusion within 60 seconds; reconnecting for chain state"
        );
    }
    Ok(true)
}
async fn session(shared: Arc<Shared>, backend: Arc<AsicBackend>) -> Result<()> {
    shared.runtime.lock().expect("runtime").transition(
        "waiting_for_node",
        "connecting",
        "Connecting to your full node…",
    );
    let resolved = chain(&shared.config)?
        .resolve()
        .map_err(anyhow::Error::msg)?;
    let minimum_deficit = resolved.constants.min_blocks_per_challenge_block;
    let bits = resolved.constants.discriminant_size_bits as usize;
    let mut scheduler = Scheduler::new(resolved.constants, resolved.allows_bootstrap, None)?;
    let mut socket = connect(&shared.config).await?;
    shared.runtime.lock().expect("runtime").connected();
    shared.runtime.lock().expect("runtime").transition(
        "waiting_for_node",
        "authenticated",
        "Connected. Waiting for chain state from your node.",
    );
    let mut workers: JoinSet<Result<Completed>> = JoinSet::new();
    let mut primers: JoinSet<Result<()>> = JoinSet::new();
    // JoinSet aborts all workers on session exit. Interrupted I/O closes its lease.
    let mut active: Option<WorkPlan> = None;
    let mut ready: Option<Completed> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    let mut last_message = Instant::now();
    let connected = Instant::now();
    let mut infusion_submitted_at = None;
    loop {
        let awaiting_confirmation = waiting_for_infusion_confirmation(
            scheduler.needs_resync(),
            infusion_submitted_at,
            Instant::now(),
        )?;
        let next = if awaiting_confirmation {
            None
        } else {
            scheduler.next_plan()?
        };
        if active.as_ref().is_some_and(|current| {
            next.as_ref().is_none_or(|next| {
                current.generation != next.generation
                    || current.total_iters != next.total_iters
                    || current.event != next.event
            })
        }) {
            workers.abort_all();
            while workers.join_next().await.is_some() {}
            active = None;
            ready = None;
        }
        if active.is_none()
            && let Some(plan) = next
        {
            {
                let mut state = shared.runtime.lock().expect("runtime");
                if state.phase != "running" {
                    state.transition(
                        "waiting_for_clients",
                        "ready",
                        "Waiting for verified ASIC proofs.",
                    );
                }
            }
            active = Some(plan.clone());
            workers.spawn(execute(backend.clone(), plan));
        }
        if ready
            .as_ref()
            .is_some_and(|r| Instant::now() >= r.plan.not_before)
            && let Some(completed) = ready.take()
        {
            // finish independently verifies full checkpointed proofs and request identity.
            if let Some(output) = scheduler.finish(completed.plan, completed.result)? {
                let next_infused = match &output {
                    Output::EndOfSubSlot(value) => {
                        let slot = &value.end_of_sub_slot_bundle;
                        let input = if slot.reward_chain.deficit < minimum_deficit {
                            Some(dg_xch_timelord::worker::ProofRequest { generation:scheduler.generation(),challenge:slot.infused_challenge_chain.as_ref().context("Missing infused slot input")?.hash()?,input:dg_xch_core::blockchain::class_group_element::ClassgroupElement::get_default_element(),iterations:1,discriminant_bits:bits })
                        } else {
                            None
                        };
                        Some(input)
                    }
                    _ => None,
                };
                let infusion = matches!(&output, Output::InfusionPoint(_));
                let message = match output {
                    Output::SignagePoint(value) => {
                        tracing::info!(index=value.index_from_challenge, cc=%value.challenge_chain_sp_vdf.output.hash()?, rc=%value.reward_chain_sp_vdf.output.hash()?, cc_proof=%proof_digest(&value.challenge_chain_sp_proof)?, rc_proof=%proof_digest(&value.reward_chain_sp_proof)?, "Submitting verified signage point");
                        encode(ProtocolMessageTypes::NewSignagePointVdf, &value, None)?
                    }
                    Output::InfusionPoint(value) => {
                        tracing::info!(unfinished_reward_hash=%value.unfinished_reward_hash, "Submitting verified infusion point");
                        encode(ProtocolMessageTypes::NewInfusionPointVdf, &value, None)?
                    }
                    Output::EndOfSubSlot(value) => {
                        encode(ProtocolMessageTypes::NewEndOfSubSlotVdf, &value, None)?
                    }
                };
                send(&mut socket, message).await?;
                if infusion {
                    infusion_submitted_at = Some(Instant::now());
                }
                shared
                    .runtime
                    .lock()
                    .expect("runtime")
                    .delivered(completed.speeds);
                if let Some(work) = next_infused {
                    prime_infused(&mut primers, backend.clone(), work);
                }
            }
            active = None;
            continue;
        }
        tokio::select! {
            _ = tick.tick() => {
                if !scheduler.initialized() && connected.elapsed() > Duration::from_secs(30) { bail!("Node has not supplied chain state yet"); }
            }
            _ = heartbeat.tick() => {
                if last_message.elapsed() > Duration::from_secs(90) { bail!("Full-node heartbeat expired"); }
                send(&mut socket, Message::Ping(Vec::new().into())).await?;
            }
            Some(completed) = primers.join_next(), if !primers.is_empty() => {
                match completed { Ok(result) => result?, Err(error) if error.is_cancelled() => {}, Err(error) => return Err(error.into()) }
            }
            Some(completed) = workers.join_next(), if !workers.is_empty() => {
                match completed { Ok(Ok(result)) if result.plan.generation == scheduler.generation() => ready = Some(result), Ok(Ok(_)) => {}, Ok(Err(error)) => return Err(error), Err(error) if error.is_cancelled() => {}, Err(error) => return Err(error.into()) }
            }
            incoming = socket.next() => {
                last_message = Instant::now();
                match incoming.context("Node disconnected")?? {
                    Message::Binary(bytes) => {
                        let message: ChiaMessage = decode_exact(&bytes)?;
                        match message.msg_type {
                            ProtocolMessageTypes::NewPeakTimelord => {
                                let peak: dg_xch_core::protocols::timelord::NewPeakTimelord=decode_exact(message.data.as_slice())?;
                                if scheduler.set_peak(peak.clone())? {
                                    infusion_submitted_at = None;
                                    let work=infused_from_peak(&peak,scheduler.generation(),bits,minimum_deficit)?;
                                    prime_infused(&mut primers,backend.clone(),work);
                                }
                            }
                            ProtocolMessageTypes::NewGenesisTimelord => { if scheduler.set_genesis(decode_exact(message.data.as_slice())?)? { prime_infused(&mut primers,backend.clone(),None); } }
                            ProtocolMessageTypes::NewUnfinishedBlockTimelord => { if let Err(error) = scheduler.add_unfinished(decode_unfinished(message.data.as_slice()).context("Decoding unfinished block")?) { tracing::warn!(%error, "Rejected unfinished block"); } }
                            ProtocolMessageTypes::RequestCompactProofOfTime => {},
                            _ => bail!("Unexpected node protocol message"),
                        }
                    }
                    Message::Ping(data) => send(&mut socket, Message::Pong(data)).await?,
                    Message::Pong(_) => {}, Message::Close(_) => bail!("Node closed connection"), _ => bail!("Unexpected WebSocket frame"),
                }
            }
        }
    }
}
pub async fn run(
    shared: Arc<Shared>,
    backend: Arc<AsicBackend>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let result = tokio::select! { _ = shutdown.changed() => break, result = session(shared.clone(), backend.clone()) => result };
        if let Err(error) = result {
            tracing::warn!(%error, "Timelord connection stopped");
            let mut state = shared.runtime.lock().expect("runtime");
            if let Some(failure) = error.downcast_ref::<AsicFailure>() {
                state
                    .boards
                    .entry(failure.board_id.clone())
                    .or_default()
                    .state = "fault".into();
                if failure.verification {
                    state.failed_proofs += 1;
                }
                state.event(Some(failure.board_id.clone()), "board_fault");
            }
            state.transition("waiting_for_node", "disconnected", &error.to_string());
            state.event(None, "node_unavailable");
        }
        backend.stop_all().await;
        tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
    }
    backend.stop_all().await;
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let current = include_bytes!("../tests/fixtures/unfinished-block.bin");
        let decoded = decode_unfinished(current).unwrap();
        assert_eq!(decoded.difficulty, 2416);
        assert_eq!(decoded.sub_slot_iters, 587202560);
        let legacy = &current[..current.len() - 1];
        assert_eq!(decode_unfinished(legacy).unwrap(), decoded);
        assert!(decode_unfinished(&legacy[..legacy.len() - 1]).is_err());
        let mut mmr = legacy.to_vec();
        mmr.push(1);
        mmr.extend([42; 32]);
        assert!(
            decode_unfinished(&mmr)
                .unwrap_err()
                .to_string()
                .contains("MMR")
        );
        for extension in [vec![2], vec![0, 0], vec![1], vec![1; 34]] {
            let mut malformed = legacy.to_vec();
            malformed.extend(extension);
            assert!(decode_unfinished(&malformed).is_err());
        }
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
        assert!(
            validate_handshake(ProtocolMessageTypes::Handshake, &greeting, "testnet11").is_err()
        );
        greeting.node_type = NodeType::Farmer as u8;
        assert!(validate_handshake(ProtocolMessageTypes::Handshake, &greeting, "mainnet").is_err());
        assert!(
            validate_handshake(ProtocolMessageTypes::NewPeakTimelord, &greeting, "mainnet")
                .is_err()
        );
    }
    async fn fixture(server_name: &str, board_fault: bool) -> Result<()> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut parameters = CertificateParams::new(vec!["fixture-ca".into()])?;
        parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate()?;
        let ca = parameters.self_signed(&ca_key)?;
        let server_key = KeyPair::generate()?;
        let server = CertificateParams::new(vec!["chia.net".into()])?.signed_by(
            &server_key,
            &ca,
            &ca_key,
        )?;
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
        config.node.network = timelord_core::Network::Testnet11;
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
            assert_eq!(greeting.network_id, "testnet11");
            socket
                .send(encode(
                    ProtocolMessageTypes::Handshake,
                    &handshake("testnet11", NodeType::FullNode, 8444),
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
            let runtime = Arc::new(std::sync::Mutex::new(RuntimeState::default()));
            runtime.lock().unwrap().boards.insert(
                "failed".into(),
                HardwareState {
                    state: "fault".into(),
                    latched_fault: true,
                    ..Default::default()
                },
            );
            let (backend, _offers) = AsicBackend::new(
                &config,
                &config.assignments().map_err(anyhow::Error::msg)?,
                runtime.clone(),
            )?;
            let shared = Arc::new(Shared {
                assignments: config.assignments().map_err(anyhow::Error::msg)?,
                config,
                clients: Default::default(),
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
    async fn authenticates_a_mutual_tls_chia_websocket_with_a_fixture_only_identity() {
        fixture("chia.net", false).await.unwrap();
    }
    #[tokio::test]
    async fn rejects_an_incorrect_tls_server_name() {
        assert!(fixture("wrong.invalid", false).await.is_err());
    }
    #[tokio::test]
    async fn full_node_session_keeps_answering_while_a_board_is_latched_off() {
        fixture("chia.net", true).await.unwrap();
    }
}

//! Native coordinator: upstream scheduler/transport, persistent existing ASIC clients.
use crate::{
    Shared,
    asic::{AsicBackend, ProofTiming},
    observability::{PeakResult, ProofStage, SubmissionKind, UnfinishedOutcome, UnfinishedSource},
};
use anyhow::{Context, Result, bail};
use dg_xch_core::protocols::{ChiaMessage, NodeType, ProtocolMessageTypes, shared::Handshake};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use dg_xch_servers::transport::{
    SERVICE_MESSAGE_LIMIT, TlsIdentity, decode_exact, encode, handshake,
};
use dg_xch_timelord::scheduler::{Admission, Output, Scheduler, WorkPlan, WorkResult};
use futures_util::{SinkExt, StreamExt};
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
mod state;
pub(crate) use state::RuntimeScope;
pub use state::{Event, HardwareState, Observation, RawSpeed, RawState, RuntimeState};

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
    let network = config.node.network.label().to_owned();
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
            &timelord_handshake(&network),
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
fn timelord_handshake(network: &str) -> Handshake {
    let mut greeting = handshake(network, NodeType::Timelord, 0);
    if !greeting
        .capabilities
        .iter()
        .any(|capability| capability.0 == 6)
    {
        greeting.capabilities.push((6, "1".to_owned()));
    }
    greeting
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
// Current envelopes carry the MMR root. Retain exact legacy framing only when
// appending the absent-root byte produces a complete, valid message.
fn decode_unfinished(
    bytes: &[u8],
) -> Result<dg_xch_core::protocols::timelord::NewUnfinishedBlockTimelord> {
    if bytes.len() > SERVICE_MESSAGE_LIMIT {
        bail!("Unfinished block exceeds message limit");
    }
    if let Ok(value) = decode_exact(bytes) {
        return Ok(value);
    }
    let mut legacy = bytes.to_vec();
    legacy.push(0);
    let value: dg_xch_core::protocols::timelord::NewUnfinishedBlockTimelord =
        decode_exact(&legacy).context("Malformed unfinished-block envelope")?;
    if value.header_mmr_root.is_some() {
        bail!("Truncated unfinished-block MMR root");
    }
    Ok(value)
}

fn admission_outcome(admission: Admission) -> UnfinishedOutcome {
    match admission {
        Admission::Admitted(_) => UnfinishedOutcome::Admitted,
        Admission::Deferred => UnfinishedOutcome::Deferred,
        Admission::DeferredQueueFull => UnfinishedOutcome::DeferredQueueFull,
        Admission::CandidatesFull => UnfinishedOutcome::CandidatesFull,
        Admission::Duplicate => UnfinishedOutcome::Duplicate,
        Admission::Stale => UnfinishedOutcome::Stale,
        Admission::Ignored => UnfinishedOutcome::Ignored,
    }
}
fn log_admission(admission: Admission, proof_version: Option<u8>, source: &'static str) {
    match admission {
        Admission::Admitted(hash) => match proof_version {
            Some(proof_version) => {
                tracing::info!(proof_version,%hash,source,"Scheduled validated unfinished block")
            }
            None => tracing::info!(%hash,source,"Scheduled validated unfinished block"),
        },
        Admission::Deferred => {
            tracing::debug!(
                source,
                "Deferred unfinished block until chain context is known"
            )
        }
        Admission::DeferredQueueFull => {
            tracing::warn!(source, "Deferred unfinished block queue is full")
        }
        Admission::CandidatesFull => tracing::warn!(
            source,
            "Unfinished block matches the current peak but the scheduled-candidate set is full"
        ),
        Admission::Duplicate => tracing::debug!(source, "Ignored duplicate unfinished block"),
        Admission::Stale => tracing::debug!(source, "Ignored stale unfinished block"),
        Admission::Ignored => {
            tracing::debug!(
                source,
                "Ignored unfinished block without matching chain state"
            )
        }
    }
}
fn handle_retry_outcome(
    (hash, outcome): (
        dg_xch_core::blockchain::sized_bytes::Bytes32,
        Result<Admission, String>,
    ),
    metrics: &crate::observability::Metrics,
) {
    match outcome {
        Ok(admission) => {
            log_admission(admission, None, "retry");
            metrics.unfinished_candidate(admission_outcome(admission), UnfinishedSource::Retry);
        }
        Err(error) => {
            metrics.unfinished_candidate(UnfinishedOutcome::Rejected, UnfinishedSource::Retry);
            tracing::warn!(%hash,%error,"Rejected unfinished block");
        }
    }
}
// Candidate decoding, commitment hashing and admission share a recoverable boundary.
// Malformed node frames are deliberately non-fatal: they are logged and dropped
// without resetting the scheduler or the authenticated node session.
fn handle_unfinished(
    scheduler: &mut dg_xch_timelord::scheduler::Scheduler,
    bytes: &[u8],
    metrics: &crate::observability::Metrics,
) -> Result<Admission> {
    let result = (|| -> Result<_> {
        let candidate = decode_unfinished(bytes).context("Decoding unfinished block")?;
        let version = candidate.reward_chain_block.proof_of_space.version;
        Ok((version, scheduler.admit_unfinished(candidate)?))
    })();
    match result {
        Ok((proof_version, admission)) => {
            log_admission(admission, Some(proof_version), "node");
            metrics.unfinished_candidate(admission_outcome(admission), UnfinishedSource::Node);
            Ok(admission)
        }
        Err(error) => {
            metrics.unfinished_candidate(UnfinishedOutcome::Rejected, UnfinishedSource::Node);
            tracing::warn!(%error,"Rejected unfinished block");
            Err(error)
        }
    }
}

async fn send(socket: &mut Socket, message: Message) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), socket.send(message))
        .await
        .context("Node send timed out")??;
    Ok(())
}
async fn submit(
    shared: &Shared,
    kind: SubmissionKind,
    stage: &timelord_supervisor::progress::Guard,
    sending: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    stage.stage(timelord_supervisor::progress::Stage::Submitting);
    let started = Instant::now();
    sending.await?;
    if let Some(metrics) = shared.metrics() {
        metrics.submitted(kind, started.elapsed());
    }
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
        speeds.push((Chain::InfusedChallenge, speed.clone()));
    }
    let bundle_ready = Instant::now();
    for (chain, timing) in &speeds {
        let waited = bundle_ready.saturating_duration_since(timing.ready_at);
        backend.observe_duration(*chain, ProofStage::BundleWait, waited);
        if waited > Duration::from_secs(10) {
            backend.bundle_delayed(*chain);
        }
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
    let metrics = shared
        .metrics()
        .context("Node session requires a production runtime")?;
    {
        let mut state = shared.runtime.lock().expect("runtime");
        state.metric_epoch = metrics.begin_session();
        state.scheduler_initialized = false;
        state.scheduler_height = None;
        state.scheduler_deficit = None;
    }
    shared.runtime.lock().expect("runtime").transition(
        "waiting_for_node",
        "connecting",
        "Connecting to your full node…",
    );
    let resolved = crate::network::resolve(&shared.config.node)?;
    let minimum_deficit = resolved.constants.min_blocks_per_challenge_block;
    let bits = resolved.constants.discriminant_size_bits as usize;
    let mut scheduler = Scheduler::new(resolved.constants, resolved.allows_bootstrap, None)?;
    let mut socket = connect(&shared.config).await?;
    {
        let mut state = shared.runtime.lock().expect("runtime");
        state.connected();
        metrics.connected();
    }
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
    let mut eligibility = None;
    let progress = &shared.process.progress;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    let mut last_message = Instant::now();
    let connected = Instant::now();
    let mut infusion_submitted_at = None;
    let mut last_initialized = false;
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
        let initialized = scheduler.initialized();
        if initialized != last_initialized {
            shared
                .runtime
                .lock()
                .expect("runtime")
                .scheduler_initialized = initialized;
            last_initialized = initialized;
        }
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
            eligibility = None;
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
            eligibility.take();
            let stage = progress.begin(
                timelord_supervisor::progress::Resource::Scheduler,
                completed.plan.generation,
                completed.plan.challenge.iterations,
                timelord_supervisor::progress::Stage::Scheduler,
            );
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
                let kind = match &output {
                    Output::SignagePoint(_) => SubmissionKind::Signage,
                    Output::InfusionPoint(_) => SubmissionKind::Infusion,
                    Output::EndOfSubSlot(_) => SubmissionKind::EndOfSubSlot,
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
                submit(&shared, kind, &stage, send(&mut socket, message)).await?;
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
                match completed { Ok(Ok(result)) if result.plan.generation == scheduler.generation() => {
                    let wait_seconds = result.plan.not_before.saturating_duration_since(Instant::now()).as_secs().saturating_add(1);
                    eligibility=Some(progress.begin(timelord_supervisor::progress::Resource::Eligibility,result.plan.generation,result.plan.challenge.iterations,timelord_supervisor::progress::Stage::Eligibility{wait_seconds}));
                    ready = Some(result);
                }, Ok(Ok(_)) => {}, Ok(Err(error)) => return Err(error), Err(error) if error.is_cancelled() => {}, Err(error) => return Err(error.into()) }
            }
            incoming = socket.next() => {
                last_message = Instant::now();
                if incoming.as_ref().is_some_and(|r|r.is_ok()) {metrics.node_message_received();}
                match incoming.context("Node disconnected")?? {
                    Message::Binary(bytes) => {
                        let message: ChiaMessage = decode_exact(&bytes)?;
                        match message.msg_type {
                            ProtocolMessageTypes::NewPeakTimelord => {
                                let peak: dg_xch_core::protocols::timelord::NewPeakTimelord=decode_exact(message.data.as_slice()).context("Decoding timelord peak")?;
                                metrics.peak_received();
                                let result = scheduler.set_peak(peak.clone());
                                metrics.peak_updated(match &result {
                                    Ok(true) => PeakResult::Accepted,
                                    Ok(false) => PeakResult::Ignored,
                                    Err(_) => PeakResult::Rejected,
                                });
                                let accepted = result?;
                                for outcome in scheduler.take_retry_outcomes() {
                                    handle_retry_outcome(outcome, metrics);
                                }
                                if accepted {
                                    {
                                        let mut state=shared.runtime.lock().expect("runtime");
                                        state.scheduler_initialized=scheduler.initialized();
                                        state.scheduler_height=Some(peak.reward_chain_block.height);
                                        state.scheduler_deficit=Some(peak.deficit);
                                        // A peak is the routing boundary for
                                        // optional rebalancing.
                                        state.peak_epoch = state.peak_epoch.wrapping_add(1);
                                    }
                                    infusion_submitted_at = None;
                                    let work=infused_from_peak(&peak,scheduler.generation(),bits,minimum_deficit)?;
                                    prime_infused(&mut primers,backend.clone(),work);
                                }
                            }
                            ProtocolMessageTypes::NewGenesisTimelord => { if scheduler.set_genesis(decode_exact(message.data.as_slice())?)? { prime_infused(&mut primers,backend.clone(),None); } }
                            ProtocolMessageTypes::NewUnfinishedBlockTimelord => {
                                let _ = handle_unfinished(&mut scheduler,message.data.as_slice(),metrics);
                            }
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
            // AsicBackend records board faults once, with its captured native
            // generation. A node-session relay must not fault its replacement.
            if let Some(metrics) = shared.metrics() {
                metrics.disconnect(state.metric_epoch);
            }
            state.scheduler_initialized = false;
            state.transition("waiting_for_node", "disconnected", &error.to_string());
            state.event(None, "node_unavailable");
        }
        backend.stop_all().await;
        tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
    }
    backend.stop_all().await;
}

#[cfg(test)]
#[path = "runtime/tests.rs"]
mod tests;

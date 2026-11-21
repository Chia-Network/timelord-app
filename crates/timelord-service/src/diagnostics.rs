//! Explicit hardware diagnostic. Never runs as part of normal startup or tests.
use crate::{Shared, asic::AsicBackend, hardware, runtime::RuntimeState, unix_now};
use anyhow::{Context, Result, bail};
use dg_xch_core::blockchain::{class_group_element::ClassgroupElement, sized_bytes::Bytes32};
use dg_xch_timelord::worker::ProofRequest;
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use timelord_core::{Chain, Config};
use timelord_supervisor::InstanceLock;
use tokio::{sync::watch, task::JoinSet};

/// Take the serving worker lock and inspect the safety journal without
/// persisting or archiving it. A reset journal must be recorded by a real
/// board-state save first, so boards never start over in-memory defaults.
pub(crate) fn prepare_store(config_path: &Path) -> Result<(InstanceLock, crate::safety::Store)> {
    let paths = timelord_supervisor::Paths::for_config(config_path)?;
    let worker = InstanceLock::acquire(&paths.worker_lock)
        .context("Another timelord worker is running; stop it before running check-hardware")?;
    let safety = crate::safety::Store::inspect(config_path)?;
    if safety.journal_reset().is_some() {
        bail!(
            "Board safety journal is missing or unreadable; start the worker until it has saved board state so the reset is recorded and an unreadable journal is archived before running check-hardware"
        );
    }
    Ok((worker, safety))
}

#[derive(Serialize)]
pub struct CheckedProof {
    chain: Chain,
    generation: u64,
    iterations: u64,
    checkpoint_count: u8,
    request_seconds: f64,
    session_ips: f64,
}

/// Exercise concurrent chains, retained sessions, and a challenge reset through
/// the production owner path. These synthetic proofs are verified locally and
/// never submitted to a node.
pub async fn check(mut config: Config, config_path: &Path) -> Result<Vec<CheckedProof>> {
    if config.hardware.client_binary.is_none() || config.hardware.boards.is_empty() {
        bail!("Choose a native ASIC client and explicitly bound boards first");
    }
    for board in &mut config.hardware.boards {
        if board.profile == timelord_core::ProfileMode::Automatic {
            board.frequency_mhz = board.automatic.start_frequency_mhz;
            board.voltage_v = board.automatic.start_voltage_v;
            board.profile = timelord_core::ProfileMode::Manual;
        }
    }
    let process = crate::operational::State::new(crate::observability::Metrics::new(
        config.node.network.clone(),
    )?);
    // Owner epochs and safety restrictions are physical facts the next
    // production start must see; reliability health is not changed. The worker
    // lock stays held for the whole check, including the board work below.
    let (_worker, safety) = prepare_store(config_path)?;
    let safety = Arc::new(safety);
    let mut runtime = RuntimeState::new(false);
    hardware::restore_lifecycles(&config, &mut runtime.boards, &safety);
    runtime.safety = Some(safety);
    let runtime = Arc::new(Mutex::new(runtime));
    let backend = Arc::new(AsicBackend::new(runtime.clone(), process.clone()));
    runtime.lock().expect("runtime").prover = Arc::downgrade(&backend);
    let shared = Arc::new(Shared::new(
        None,
        config,
        runtime.clone(),
        process,
        unix_now(),
        unix_now(),
    ));
    let (stop, shutdown) = watch::channel(false);
    let (_updates, receiver) = watch::channel(shared.clone());
    let mut supervisor = tokio::spawn(hardware::continuous::run(
        shared.clone(),
        backend.clone(),
        receiver,
        shutdown,
    ));
    let routed = async {
        while runtime.lock().expect("runtime").routing.len() < 3 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(120), routed)
        .await
        .context("Boards did not become ready for the hardware check")?;
    let workload = async {
        let mut checked = vec![];
        // Round two must reuse each session; round three must reset it.
        for (generation, iterations) in [(1, 1_000_000), (1, 2_000_000), (2, 1_000_000)] {
            let mut work = JoinSet::new();
            for (index, chain) in Chain::ALL.into_iter().enumerate() {
                let backend = backend.clone();
                work.spawn(async move {
                    let started = Instant::now();
                    let request = ProofRequest {
                        generation,
                        challenge: Bytes32::const_new([42 + index as u8 + generation as u8; 32]),
                        input: ClassgroupElement::get_default_element(),
                        iterations,
                        discriminant_bits: 1024,
                    };
                    let (proof, timing) = backend.prove(chain, request).await?;
                    Ok::<_, anyhow::Error>(CheckedProof {
                        chain,
                        generation,
                        iterations,
                        checkpoint_count: proof.proof.witness_type,
                        request_seconds: started.elapsed().as_secs_f64(),
                        session_ips: timing.delivered_ips(),
                    })
                });
            }
            while let Some(result) = work.join_next().await {
                checked.push(result??);
            }
        }
        Ok::<_, anyhow::Error>(checked)
    };
    let mut supervisor_finished = false;
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(180), workload) => result.context("Hardware check timed out").and_then(|result| result),
        result = &mut supervisor => {
            supervisor_finished = true;
            match result {
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
                Ok(Ok(())) => Err(anyhow::anyhow!("ASIC supervisor stopped before the check finished")),
            }
        },
        _ = crate::service::shutdown_signal() => Err(anyhow::anyhow!("Hardware check interrupted")),
    };
    backend.stop_all().await;
    let _ = stop.send(true);
    if !supervisor_finished {
        supervisor.await.context("ASIC supervisor failed")??;
    }
    result
}

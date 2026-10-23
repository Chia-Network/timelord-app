//! Explicit hardware diagnostic. Never runs as part of normal startup or tests.
use crate::{
    Shared, asic::AsicBackend, client_listener, hardware, runtime::RuntimeState, unix_now,
};
use anyhow::{Context, Result, bail};
use dg_xch_core::blockchain::{class_group_element::ClassgroupElement, sized_bytes::Bytes32};
use dg_xch_timelord::worker::ProofRequest;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use timelord_core::{Chain, Config};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

#[derive(Serialize)]
pub struct CheckedProof {
    chain: Chain,
    generation: u64,
    iterations: u64,
    checkpoint_count: u8,
    request_seconds: f64,
    session_ips: f64,
}

/// Exercise concurrent chains, retained sessions, and a challenge reset. These
/// synthetic proofs are verified locally and are never submitted to a node.
pub async fn check(mut config: Config) -> Result<Vec<CheckedProof>> {
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
    let assignments = config.assignments().map_err(anyhow::Error::msg)?;
    let mut listeners = vec![];
    for board in &config.hardware.boards {
        let capacity = assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .count() as u8;
        if capacity > 0 {
            let listener =
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, board.client_port)).await?;
            listeners.push((board.id.clone(), capacity, listener));
        }
    }
    let runtime = Arc::new(Mutex::new(RuntimeState::default()));
    let (backend, offers) = AsicBackend::new(&config, runtime.clone())?;
    let backend = Arc::new(backend);
    let shared = Arc::new(Shared {
        config,
        assignments,
        clients: Arc::new(Mutex::new(BTreeSet::new())),
        runtime,
        started_unix: unix_now(),
    });
    let (stop, shutdown) = watch::channel(false);
    let mut listeners_tasks = JoinSet::new();
    for (id, capacity, listener) in listeners {
        listeners_tasks.spawn(client_listener(
            listener,
            id.clone(),
            capacity,
            shared.clone(),
            shutdown.clone(),
            offers.get(&id).cloned(),
        ));
    }
    let mut supervisor = tokio::spawn(hardware::run(shared.clone(), backend.clone(), shutdown));
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
        _ = crate::shutdown_signal() => Err(anyhow::anyhow!("Hardware check interrupted")),
    };
    backend.stop_all().await;
    let _ = stop.send(true);
    if !supervisor_finished {
        supervisor.await.context("ASIC supervisor failed")??;
    }
    while let Some(result) = listeners_tasks.join_next().await {
        result??;
    }
    result
}

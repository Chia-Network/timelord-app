//! Supervise only explicitly configured native clients. Initialization is sequential.
mod admission;
mod board;
mod native;
mod tuning;
use crate::{
    Shared,
    automatic::{self, Action, Point, Policy},
    runtime::{HardwareState, RawState},
};
use anyhow::{Context, Result, bail};
use board::*;
pub(crate) use native::INITIALIZATION;
use native::*;
use std::{process::Stdio, sync::Arc, time::Duration};
use timelord_core::{BoardConfig, Config, ProfileMode};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch},
    task::JoinSet,
};
use tuning::*;

pub fn arguments(board: &BoardConfig, engines: usize) -> Result<Vec<String>> {
    let mask = timelord_core::engine_mask(engines).context("Expected one to three engines")?;
    let mut arguments = vec![
        "--ip".into(),
        "127.0.0.1".into(),
        "--vdfs-mask".into(),
        mask.to_string(),
        "--device-loc".into(),
        board
            .device_location
            .expect("validated location")
            .to_string(),
        board.client_port.to_string(),
        engines.to_string(),
    ];
    if let Some(serial) = &board.serial {
        arguments.extend(["--expected-serial".into(), serial.clone()]);
    }
    arguments.push("--raw-speed".into());
    let (frequency, voltage) = board.operating_point();
    arguments.extend([
        "--vdf-threads".into(),
        "4".into(),
        "--proof-threads".into(),
        "3".into(),
        "--retain-asic-values".into(),
        "--checkpoint-threshold".into(),
        "250000".into(),
        "--freq".into(),
        frequency.to_string(),
        "--voltage".into(),
        voltage.to_string(),
    ]);
    // Manual profiles retain their requested setting but need the same inherited
    // controller-lifetime and hardware-readback guard as Automatic profiles.
    arguments.push("--live-control".into());
    Ok(arguments)
}
async fn supported_frequencies(config: &Config) -> Result<Vec<f64>> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(
            config
                .hardware
                .client_binary
                .as_ref()
                .context("Choose a native client")?,
        )
        .arg("--control-frequencies")
        .kill_on_drop(true)
        .output(),
    )
    .await
    .context("Native frequency capability check timed out")??;
    if !output.status.success() || output.stdout.len() > 2048 {
        bail!("Native client could not report supported Automatic frequencies");
    }
    decode_frequencies(&output.stdout)
}

fn decode_frequencies(bytes: &[u8]) -> Result<Vec<f64>> {
    let frequencies: Vec<f64> = serde_json::from_slice(bytes)
        .context("Native client needs the bundled supported-frequency capability")?;
    if frequencies.is_empty()
        || frequencies.len() > 201
        || frequencies
            .iter()
            .any(|f| !f.is_finite() || !(200. ..=2200.).contains(f))
        || frequencies.windows(2).any(|f| f[0] >= f[1])
    {
        bail!("Native client reported invalid supported frequencies");
    }
    Ok(frequencies)
}

pub async fn validate_automatic(
    config: &Config,
    assignments: &[timelord_core::Assignment],
) -> Result<()> {
    if !config
        .hardware
        .boards
        .iter()
        .any(|b| b.profile == ProfileMode::Automatic)
    {
        return Ok(());
    }
    let binary = config
        .hardware
        .client_binary
        .as_ref()
        .context("Automatic mode requires a native client")?;
    let probe = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(binary)
            .arg("--control-version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Native control capability check timed out")??;
    if !probe.status.success() || probe.stdout != b"timelord-live-control-v1\n" {
        bail!(
            "Automatic mode needs the bundled live-control native client. Select it in board settings, or use Manual settings."
        );
    }
    let frequencies = supported_frequencies(config).await?;
    let hash = automatic::fingerprint(config)?;
    let host = automatic::host();
    for board in &config.hardware.boards {
        if board.profile == ProfileMode::Automatic {
            if board.serial.as_ref().is_none_or(|s| s.is_empty()) {
                bail!(
                    "Automatic mode needs a persistent serial for {}",
                    board.name
                );
            }
            let point = automatic::initial(
                config,
                board,
                assignments
                    .iter()
                    .filter(|a| a.board_id == board.id)
                    .count(),
                &hash,
                &host,
            )?;
            if !frequencies
                .iter()
                .any(|f| (*f - point.frequency_mhz).abs() <= 1.)
            {
                bail!(
                    "{}: Initial Automatic frequency {} MHz is not supported by this native client",
                    board.name,
                    point.frequency_mhz
                );
            }
        }
    }
    Ok(())
}

pub(crate) const TEMPERATURE_TIMEOUT_SECONDS: u64 = 60;

pub(crate) fn unavailable(shared: &Shared) -> std::collections::HashSet<String> {
    let (boards, safety) = {
        let state = shared.runtime.lock().expect("runtime");
        (state.boards.clone(), state.safety.clone())
    };
    unavailable_for(&shared.config, &boards, safety.as_deref())
}
pub(crate) fn unavailable_for(
    config: &Config,
    boards: &std::collections::HashMap<String, HardwareState>,
    safety: Option<&crate::safety::Store>,
) -> std::collections::HashSet<String> {
    config
        .hardware
        .boards
        .iter()
        .filter(|board| {
            let mut lifecycle = boards
                .get(&board.id)
                .map(|h| h.lifecycle.clone())
                .unwrap_or_default();
            if let Some(reason) = safety
                .zip(board.serial.as_deref())
                .and_then(|(store, serial)| store.board(serial))
                .and_then(|record| record.quarantine)
            {
                lifecycle.quarantined(
                    reason.into(),
                    format!("Board admission required after {reason:?}"),
                    crate::unix_now(),
                );
            }
            !lifecycle
                .capacity(crate::unix_now(), safety.is_some())
                .usable
        })
        .map(|b| b.id.clone())
        .collect()
}
pub async fn run(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    validate_automatic(&shared.config, &shared.assignments).await?;
    let mut children = JoinSet::new();
    let (stop, child_shutdown) = watch::channel(false);
    let managed = shared.runtime.lock().expect("runtime").safety.is_some();
    for board in &shared.config.hardware.boards {
        if !shared.assignments.iter().any(|a| a.board_id == board.id) && !managed {
            continue;
        }
        if shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .get(&board.id)
            .is_some_and(|b| b.lifecycle.status().latched_fault)
            && !managed
        {
            continue;
        }
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(board.id.clone())
            .or_default()
            .lifecycle
            .start_native();
        let (tx, rx) = oneshot::channel();
        children.spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            board.clone(),
            tx,
            child_shutdown.clone(),
            RestartBackoff::default(),
        ));
        // Keep normal USB initialization sequential, but one broken board must
        // not prevent the next board from starting or bring down existing owners.
        tokio::select! {
            _ = shutdown.changed() => break,
            initialized = tokio::time::timeout(Duration::from_secs(30), rx) => {
                if !matches!(initialized, Ok(Ok(()))) {
                    tracing::warn!(board=%board.name, "ASIC not ready yet; continuing other boards");
                }
            }
        }
    }
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(result) = children.join_next(), if !children.is_empty() => {
                if let Err(error) = result {
                    shared.runtime.lock().expect("runtime").process.essential_failure();
                    tracing::error!(%error, "ASIC supervisor task failed");
                    return Err(error.into());
                }
            }
        }
    }
    let _ = stop.send(true);
    while let Some(result) = children.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "ASIC shutdown failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

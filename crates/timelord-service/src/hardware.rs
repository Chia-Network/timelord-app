//! Supervise only explicitly configured native clients. Initialization is sequential.
mod board;
mod failure;
pub(crate) mod lifecycle;
use failure::classify;
pub use failure::{NativeFailure, NativeFailureKind};
pub use prepared::{Facts, prepare};
pub(crate) mod continuous;
pub(crate) mod health;
mod native;
mod prepared;
mod recovery;
mod recovery_point;
mod retirement;
pub(crate) mod routing;
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
pub(crate) use recovery_point::CAUTIOUS_CEILING_MHZ;
use recovery_point::recovery_point;
use std::{process::Stdio, sync::Arc, time::Duration};
use timelord_core::{BoardConfig, Config, ProfileMode};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::watch,
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
            .context("Native client needs a resolved device location")?
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

pub(crate) const TEMPERATURE_TIMEOUT_SECONDS: u64 = 60;
/// The pinned native client writes raw-speed records every second and
/// temperatures every few seconds. Silence this long means its event loop is
/// stuck (for example inside a blocked USB write), not merely idle.
pub(crate) const LIVENESS_TIMEOUT_SECONDS: u64 = 15;

/// The one native record that proves the client paused its engines and restored
/// its safe point. Any reader that consumes it must keep the evidence.
pub(crate) const SHUTDOWN_CONFIRMED: &str =
    "Hardware shutdown: engines_paused=1 safe_point_restored=1";

/// A proof backend for owner fixtures; it is also the runtime's recovery prover.
#[cfg(test)]
pub(crate) fn fixture_backend(shared: &Shared) -> Arc<crate::asic::AsicBackend> {
    let backend = Arc::new(crate::asic::AsicBackend::new(
        shared.runtime.clone(),
        shared.process.clone(),
    ));
    for board in &shared.config.hardware.boards {
        let _ = backend.add_board(&board.id);
    }
    shared.runtime.lock().expect("runtime").prover = Arc::downgrade(&backend);
    backend
}

/// Bind runtime board state to configured serials and their saved policy.
pub(crate) fn restore_lifecycles(
    config: &Config,
    boards: &mut std::collections::HashMap<String, HardwareState>,
    safety: &crate::safety::Store,
) {
    for board in &config.hardware.boards {
        let lifecycle = &mut boards.entry(board.id.clone()).or_default().lifecycle;
        lifecycle.bind(board.serial.as_deref());
        lifecycle.restore_policy(safety.policy(board));
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

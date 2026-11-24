//! One capability/provenance decision per configuration generation.
use super::supported_frequencies;
use crate::automatic;
use anyhow::{Context, Result, bail};
use std::{sync::Arc, time::Duration};
use timelord_core::{BoardConfig, Config, ProfileMode};
use tokio::process::Command;
#[derive(Clone)]
pub struct Facts {
    pub hash: String,
    pub host: String,
    pub frequencies: Vec<f64>,
}
impl Facts {
    pub async fn load(config: &Config) -> Result<Arc<Self>> {
        let source = config.clone();
        let identity = tokio::task::spawn_blocking(move || {
            Ok::<_, anyhow::Error>((automatic::fingerprint(&source)?, automatic::host()))
        });
        let frequencies = supported_frequencies(config).await?;
        let (hash, host) = identity.await??;
        Ok(Arc::new(Self {
            hash,
            host,
            frequencies,
        }))
    }
    pub async fn learned(
        &self,
        config: &Config,
        board: &BoardConfig,
        engines: usize,
    ) -> Result<Option<automatic::Learned>> {
        let (config, board) = (config.clone(), board.clone());
        let (hash, host) = (self.hash.clone(), self.host.clone());
        tokio::task::spawn_blocking(move || {
            automatic::learned(&config, &board, engines, &hash, &host)
        })
        .await?
    }
}
/// Validate native capabilities once per service process.
impl Facts {
    /// A start hint carried forward from an earlier native build's profile.
    pub async fn inherited(
        &self,
        config: &Config,
        board: &BoardConfig,
        engines: usize,
    ) -> Result<Option<automatic::Point>> {
        let (config, board) = (config.clone(), board.clone());
        let (hash, host, frequencies) = (
            self.hash.clone(),
            self.host.clone(),
            self.frequencies.clone(),
        );
        tokio::task::spawn_blocking(move || {
            automatic::inherited(&config, &board, engines, &hash, &host, &frequencies)
        })
        .await?
    }
}
pub async fn prepare(config: &Config) -> Result<Option<Arc<Facts>>> {
    if !config
        .hardware
        .boards
        .iter()
        .any(|b| b.profile == ProfileMode::Automatic)
    {
        return Ok(None);
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
    let facts = Facts::load(config).await?;
    for board in config
        .hardware
        .boards
        .iter()
        .filter(|b| b.profile == ProfileMode::Automatic)
    {
        if board.serial.as_ref().is_none_or(String::is_empty) {
            bail!(
                "Automatic mode needs a persistent serial for {}",
                board.name
            );
        }
        let start = board.automatic.start_frequency_mhz;
        if !facts.frequencies.iter().any(|f| (*f - start).abs() <= 1.) {
            bail!(
                "{}: Automatic start frequency {start} MHz is not supported by this native client",
                board.name
            );
        }
    }
    Ok(Some(facts))
}

#[cfg(test)]
mod tests;

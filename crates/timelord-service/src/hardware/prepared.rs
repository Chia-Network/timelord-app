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
pub struct Prepared {
    pub assignments: Vec<timelord_core::Assignment>,
    pub native: Option<Arc<Facts>>,
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
pub async fn prepare(config: &Config) -> Result<Prepared> {
    let native = if config
        .hardware
        .boards
        .iter()
        .any(|b| b.profile == ProfileMode::Automatic)
    {
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
        Some(Facts::load(config).await?)
    } else {
        None
    };
    let (source, facts) = (config.clone(), native.clone());
    let assignments = tokio::task::spawn_blocking(move || {
        let identity = facts.as_ref().map(|f| (f.hash.as_str(), f.host.as_str()));
        let assignments = crate::assignment::resolve_with_identity(&source, identity)?;
        if let Some(facts) = facts {
            for board in source.hardware.boards.iter()
                .filter(|b| b.profile == ProfileMode::Automatic)
            {
                if board.serial.as_ref().is_none_or(String::is_empty) {
                    bail!("Automatic mode needs a persistent serial for {}", board.name);
                }
                let engines = assignments.iter().filter(|a| a.board_id == board.id).count();
                let point = automatic::initial(&source, board, engines, &facts.hash, &facts.host)?;
                if !facts.frequencies.iter().any(|f| (*f - point.frequency_mhz).abs() <= 1.) {
                    bail!(
                        "{}: Initial Automatic frequency {} MHz is not supported by this native client",
                        board.name, point.frequency_mhz
                    );
                }
            }
        }
        Ok::<_, anyhow::Error>(assignments)
    })
    .await??;
    Ok(Prepared {
        assignments,
        native,
    })
}

#[cfg(test)]
mod tests;

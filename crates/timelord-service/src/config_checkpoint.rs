//! Preserve the configuration that actually submitted verified work. Invalid
//! operator edits remain untouched; a fallback is explicit in status/progress.
use crate::{Shared, observability};
use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use timelord_core::Config;
use tokio::sync::watch;
const LIMIT: u64 = 1024 * 1024;

/// Configuration text failures. Only an unreadable file may fall back to the
/// last-good checkpoint; malformed or oversized content must be reported.
#[derive(Debug)]
pub enum ConfigTextError {
    Unreadable(std::io::Error),
    Invalid(String),
}
impl std::fmt::Display for ConfigTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(error) => error.fmt(f),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for ConfigTextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

/// Read at most `LIMIT + 1` bytes so the exact limit is testable independently
/// of metadata (a file can grow between the stat and the read).
fn read_bounded(reader: impl Read) -> Result<Vec<u8>, ConfigTextError> {
    let mut bytes = Vec::new();
    reader
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(ConfigTextError::Unreadable)?;
    if bytes.len() as u64 > LIMIT {
        return Err(ConfigTextError::Invalid(
            "Configuration exceeds the 1 MiB size limit".into(),
        ));
    }
    Ok(bytes)
}

pub fn bounded_text(path: &Path, private: bool) -> Result<String, ConfigTextError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_CLOEXEC | libc::O_NONBLOCK | if private { libc::O_NOFOLLOW } else { 0 },
        )
        .open(path)
        .map_err(ConfigTextError::Unreadable)?;
    let meta = file.metadata().map_err(ConfigTextError::Unreadable)?;
    if !meta.is_file() || meta.len() > LIMIT {
        return Err(ConfigTextError::Invalid(
            "Configuration must be a regular file no larger than 1 MiB".into(),
        ));
    }
    if private
        && !(meta.uid() == unsafe { libc::geteuid() }
            && meta.nlink() == 1
            && meta.mode() & 0o077 == 0)
    {
        return Err(ConfigTextError::Invalid(
            "Saved configuration must be private and owned".into(),
        ));
    }
    // Decode directly: `read_to_string` would hide malformed content behind the
    // same I/O error kind that justifies a last-good fallback.
    String::from_utf8(read_bounded(file)?).map_err(|error| {
        ConfigTextError::Invalid(format!("Configuration is not valid UTF-8: {error}"))
    })
}
fn saved_path(path: &Path) -> Result<PathBuf> {
    Ok(timelord_supervisor::Paths::for_config(path)?
        .directory
        .join("last-good.toml"))
}
/// Network intent only, so other invalid fields cannot hide the requested chain.
#[derive(Default, serde::Deserialize)]
struct RequestedNetwork {
    #[serde(default)]
    node: Option<toml::Value>,
    #[serde(default)]
    connections: Option<RequestedConnections>,
}
#[derive(Default, serde::Deserialize)]
struct RequestedConnections {
    #[serde(default)]
    active_id: Option<String>,
    #[serde(default)]
    profiles: Vec<RequestedProfile>,
}
#[derive(serde::Deserialize)]
struct RequestedProfile {
    id: String,
    node: toml::Value,
}
#[derive(Default, serde::Deserialize)]
struct NetworkOnly {
    #[serde(default)]
    network: timelord_core::Network,
    #[serde(default)]
    network_definition: Option<timelord_core::NetworkDefinition>,
}
impl NetworkOnly {
    fn chain_identity(&self) -> Result<timelord_core::ChainIdentity, String> {
        timelord_core::ChainIdentity::new(&self.network, self.network_definition.as_ref())
    }
}
impl timelord_core::ProfileId for RequestedProfile {
    fn profile_id(&self) -> &str {
        &self.id
    }
}
impl RequestedNetwork {
    /// Profiles are the persisted source of truth: their active profile carries
    /// the network intent, and a `[node]` table must equal it when present. The
    /// tables are compared verbatim so a partial or hand-edited `[node]` cannot
    /// hide behind a network-only match. An unresolvable active profile is never
    /// treated as mainnet.
    fn network(self) -> Result<NetworkOnly> {
        let Some(connections) = self.connections else {
            return intent(self.node);
        };
        let Some(profile) =
            timelord_core::select_profile(connections.active_id.as_deref(), &connections.profiles)
                .map_err(anyhow::Error::msg)?
        else {
            return intent(self.node);
        };
        if let Some(node) = self.node
            && node != profile.node
        {
            return Err(anyhow::Error::msg(timelord_core::STRAY_NODE));
        }
        intent(Some(profile.node.clone()))
    }
}
fn intent(node: Option<toml::Value>) -> Result<NetworkOnly> {
    match node {
        Some(node) => node
            .try_into()
            .context("Unreadable requested network intent"),
        None => Ok(NetworkOnly::default()),
    }
}
pub fn load_running(path: &Path) -> Result<(Config, bool)> {
    match bounded_text(path, false) {
        Ok(text) => match crate::parse_config(path, &text) {
            Ok(config) => Ok((config, false)),
            Err(requested) => {
                // Parse only network intent, even when another field is invalid.
                // Unparseable or unknown intent must not silently select a chain.
                let intent: RequestedNetwork = toml::from_str(&text).with_context(|| {
                    format!("Cannot establish requested network; fallback refused: {requested:#}")
                })?;
                let intent = intent.network().map_err(|error| {
                    anyhow::anyhow!(
                        "Cannot establish requested network; fallback refused: {requested:#}: {error:#}"
                    )
                })?;
                load_saved(path, Some(intent), requested)
            }
        },
        Err(ConfigTextError::Unreadable(error)) => load_saved(path, None, error.into()),
        Err(error @ ConfigTextError::Invalid(_)) => Err(error.into()),
    }
}
fn load_saved(
    path: &Path,
    network: Option<NetworkOnly>,
    requested: anyhow::Error,
) -> Result<(Config, bool)> {
    let saved = saved_path(path)?;
    let text = bounded_text(&saved, true).context(
        "Requested configuration invalid and no readable last-good checkpoint is available",
    )?;
    // Checkpoint storage does not change the original relative-path context.
    let config = crate::parse_config(path, &text).with_context(|| {
        format!("Requested configuration and last-good checkpoint are invalid: {requested:#}")
    })?;
    if let Some(intent) = network {
        let requested_identity = intent.chain_identity().map_err(anyhow::Error::msg)?;
        let saved_identity = config.node.chain_identity().map_err(anyhow::Error::msg)?;
        ensure!(
            requested_identity == saved_identity,
            "Requested network differs from last-good checkpoint; fallback refused: {requested:#}"
        );
    }
    tracing::warn!(
        "Requested configuration unavailable; using saved last-good configuration. Run validate to inspect the requested file."
    );
    Ok((config, true))
}
// Optional disk work must not join Tokio's blocking pool: runtime destruction
// waits indefinitely for a running fsync, even after its JoinHandle is aborted.
// The actual writer retains its process-wide permit until it really finishes.
async fn write_until_shutdown(
    write: impl FnOnce() -> Result<bool> + Send + 'static,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<bool>> {
    if *shutdown.borrow() {
        return Ok(None);
    }
    let rx = crate::optional_io::spawn("config-checkpoint", write)?;
    tokio::select! {
        _ = shutdown.changed() => Ok(None),
        result = rx => result.context("Optional configuration writer stopped")?.map(Some),
    }
}
fn eligible(shared: &Shared) -> bool {
    let sent = {
        let state = shared.runtime.lock().expect("runtime");
        state.proofs_delivered > 0 && state.failed_proofs == 0
    };
    sent && observability::work_ready(shared)
}
fn eligible_for_write(
    shared: &Arc<Shared>,
    current: &watch::Receiver<Arc<Shared>>,
    shutdown: &watch::Receiver<bool>,
) -> bool {
    let stopped = *shutdown.borrow();
    let latest = current.borrow().clone();
    !stopped && Arc::ptr_eq(shared, &latest) && eligible(shared)
}
pub async fn record(
    candidate: Config,
    path: PathBuf,
    current: watch::Receiver<Arc<Shared>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {_=shutdown.changed()=>return Ok(()),_=timer.tick()=>{}}
        let shared = current.borrow().clone();
        if !eligible(&shared) {
            continue;
        }
        let permit = tokio::select! {p=shared.process.progress.arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint).acquire_owned()=>p?,_=shutdown.changed()=>return Ok(())};
        if !eligible_for_write(&shared, &current, &shutdown) {
            continue;
        }
        let candidate = candidate.clone();
        let path = path.clone();
        let writer_current = current.clone();
        let writer_shutdown = shutdown.clone();
        let writing = shared.clone();
        match write_until_shutdown(
            move || {
                let _permit = permit;
                // Capacity waits and thread scheduling may delay this job.
                // Inspect current evidence at the actual I/O boundary, then release
                // all runtime/watch locks before touching the filesystem.
                if !eligible_for_write(&writing, &writer_current, &writer_shutdown) {
                    return Ok(false);
                }
                let _stage = writing.process.progress.begin(
                    timelord_supervisor::progress::Resource::OptionalCheckpoint,
                    0,
                    0,
                    timelord_supervisor::progress::Stage::OptionalPersistence,
                );
                crate::save(&saved_path(&path)?, &candidate).map(|()| true)
            },
            &mut shutdown,
        )
        .await
        {
            Ok(Some(true)) => {
                if eligible_for_write(&shared, &current, &shutdown) {
                    shared.process.progress.checkpointed();
                }
                return Ok(());
            }
            Ok(Some(false)) => continue,
            Ok(None) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error,"Last-good configuration checkpoint unavailable; proof work continues")
            }
        }
        tokio::select! {_=shutdown.changed()=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(60))=>{}}
    }
}
#[cfg(test)]
mod tests;

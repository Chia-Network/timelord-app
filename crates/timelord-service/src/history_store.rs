//! Bounded, atomic chart checkpoints. Restored records never become live health.
use crate::{
    Shared,
    history_buffer::LIMIT as MAX_RECORDS,
    runtime::{Event, Observation, RuntimeState},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use timelord_core::Config;
use tokio::sync::watch;

const DAY: u64 = 86400;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capture {
    pub start_unix: u64,
    pub end_unix: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    schema_version: u32,
    scope: String,
    pub observations: crate::history_buffer::History<Observation>,
    // Older checkpoints contain delivery timings only; never reinterpret them
    // as exporter-compatible measurements.
    #[serde(default)]
    pub estimated_observations: crate::history_buffer::History<Observation>,
    pub events: crate::history_buffer::History<Event>,
    pub capture_intervals: Vec<Capture>,
}

pub struct Store {
    path: PathBuf,
    scope: String,
    config: Config,
}

pub fn scope(config: &Config, assignments: &[timelord_core::Assignment]) -> Result<String> {
    // Changing a node/assignment must not attribute another session's measurements.
    let mut bytes = serde_json::to_vec(&(
        &config.node.network,
        &config.node.host,
        config.node.port,
        assignments,
    ))?;
    if config.node.network_definition.is_some() {
        let identity = timelord_core::ChainIdentity::new(
            &config.node.network,
            config.node.network_definition.as_ref(),
        )
        .map_err(anyhow::Error::msg)?;
        bytes.extend(serde_json::to_vec(&identity)?);
    }
    Ok(dg_xch_core::utils::hash_256(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

impl Store {
    pub fn new(
        path: PathBuf,
        config: &Config,
        assignments: &[timelord_core::Assignment],
    ) -> Result<Self> {
        Ok(Self {
            path,
            scope: scope(config, assignments)?,
            config: config.clone(),
        })
    }

    pub fn restore(&self, state: &mut RuntimeState, started: u64) -> Result<()> {
        let mut bytes = Vec::new();
        match fs::File::open(&self.path) {
            Ok(file) => file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "History checkpoint is too large"
        );
        let mut saved: Snapshot =
            serde_json::from_slice(&bytes).context("Reading chart checkpoint")?;
        ensure!(
            saved.schema_version == 1,
            "Unsupported history checkpoint version"
        );
        if saved.scope != self.scope {
            return Ok(());
        }
        saved.prune(started);
        state.observations_created = saved
            .observations
            .iter()
            .chain(saved.estimated_observations.iter())
            .map(|o| o.id)
            .max()
            .unwrap_or(0);
        state.events_created = saved.events.iter().map(|e| e.id).max().unwrap_or(0);
        if let Some(end) = saved.capture_intervals.last().map(|c| c.end_unix) {
            for event in saved.events.iter_mut() {
                if event.end_unix.is_none()
                    && !["recovery", "profile_change"].contains(&event.kind.as_str())
                {
                    event.end_unix = Some(end.max(event.timestamp_unix));
                }
            }
            state.event(None, "telemetry_gap");
            let gap = state.events.last_mut().expect("new restart event");
            gap.timestamp_unix = end;
            gap.end_unix = Some(started);
        }
        state.archive = Some(saved);
        Ok(())
    }

    pub fn capture(&self, state: &RuntimeState, started: u64, now: u64) -> Snapshot {
        let current_scope = state
            .routing
            .as_ref()
            .and_then(|routes| scope(&self.config, routes).ok())
            .unwrap_or_else(|| self.scope.clone());
        Snapshot::capture(current_scope, state, started, now)
    }

    pub fn checkpoint(
        self: &Arc<Self>,
        saved: Snapshot,
        progress: Arc<timelord_supervisor::progress::Progress>,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<()>>> {
        let permit = progress
            .arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint)
            .try_acquire_owned()
            .context("Optional checkpoint writer is busy")?;
        self.write_checkpoint(saved, progress, permit)
    }

    pub async fn flush(
        self: &Arc<Self>,
        saved: Snapshot,
        progress: Arc<timelord_supervisor::progress::Progress>,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let permit = progress
                .arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint)
                .acquire_owned()
                .await?;
            self.write_checkpoint(saved, progress, permit)?
                .await
                .context("History writer stopped")?
        })
        .await
        .context("Final history checkpoint timed out")?
    }

    fn write_checkpoint(
        self: &Arc<Self>,
        saved: Snapshot,
        progress: Arc<timelord_supervisor::progress::Progress>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<()>>> {
        let store = self.clone();
        crate::optional_io::spawn("history-checkpoint", move || {
            let _permit = permit;
            let _stage = progress.begin(
                timelord_supervisor::progress::Resource::OptionalCheckpoint,
                0,
                0,
                timelord_supervisor::progress::Stage::OptionalPersistence,
            );
            store.save(&saved)
        })
    }

    pub fn save(&self, saved: &Snapshot) -> Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec(saved)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "History checkpoint is too large"
        );
        timelord_supervisor::atomic::write(&self.path, &bytes)
    }
}

impl Snapshot {
    pub fn capture(scope: String, state: &RuntimeState, started: u64, now: u64) -> Self {
        let mut saved = state.archive.clone().unwrap_or_else(|| Snapshot {
            schema_version: 1,
            scope,
            observations: Default::default(),
            estimated_observations: Default::default(),
            events: Default::default(),
            capture_intervals: vec![],
        });
        saved.observations.append(&state.observations);
        saved
            .estimated_observations
            .append(&state.estimated_observations);
        saved.events.append(&state.events);
        if now > started {
            saved.capture_intervals.push(Capture {
                start_unix: started,
                end_unix: now,
            });
        }
        saved
    }
    pub fn pruned(mut self, now: u64) -> Self {
        self.prune(now);
        self
    }

    fn prune(&mut self, now: u64) {
        let cutoff = now.saturating_sub(DAY);
        for observations in [&mut self.observations, &mut self.estimated_observations] {
            observations.retain(|o| {
                o.timestamp_unix >= cutoff
                    && o.timestamp_unix <= now
                    && o.work_started_unix <= o.timestamp_unix
                    && o.ips.is_finite()
                    && o.ips >= 0.
            });
        }
        self.events.retain_mut(|e| {
            let instant = ["recovery", "profile_change"].contains(&e.kind.as_str());
            let end = if instant {
                e.timestamp_unix
            } else {
                e.end_unix.unwrap_or(now)
            };
            if e.timestamp_unix > now || end < e.timestamp_unix || end < cutoff {
                return false;
            }
            e.timestamp_unix = e.timestamp_unix.max(cutoff);
            e.end_unix = e.end_unix.map(|end| end.min(now));
            true
        });
        let capture_cutoff = if self.observations.len() == MAX_RECORDS {
            cutoff.max(
                self.observations
                    .first()
                    .map(|o| o.timestamp_unix)
                    .unwrap_or(cutoff),
            )
        } else {
            cutoff
        };
        self.capture_intervals.retain_mut(|c| {
            c.start_unix = c.start_unix.max(capture_cutoff);
            c.end_unix = c.end_unix.min(now);
            c.end_unix > c.start_unix
        });
        self.capture_intervals
            .drain(..self.capture_intervals.len().saturating_sub(MAX_RECORDS));
    }
}

pub async fn record(
    shared: Arc<Shared>,
    store: Arc<Store>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut timer = tokio::time::interval(Duration::from_secs(30));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = timer.tick() => {},
        };
        let now = crate::unix_now();
        let snapshot = store.capture(
            &shared.runtime.lock().expect("runtime"),
            shared.started_unix,
            now,
        );
        let snapshot = snapshot.pruned(now);
        let progress = shared.process.progress.clone();
        let write = match store.checkpoint(snapshot, progress) {
            Ok(write) => write,
            Err(error) => {
                tracing::warn!(%error, "Chart history checkpoint deferred");
                continue;
            }
        };
        tokio::select! {
            _ = shutdown.changed() => break,
            result = write => {
                if let Err(error) = result.context("Chart history writer stopped").and_then(|r| r) {
                    tracing::warn!(%error, "Chart history could not be saved");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelord_core::Chain;

    fn fixture() -> (PathBuf, Store) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("timelord-history-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let store = Store::new(
            directory.join("config.history.json"),
            &Config::default(),
            &[],
        )
        .unwrap();
        (directory, store)
    }

    fn proof(id: u64, timestamp: u64, ips: f64) -> Observation {
        Observation {
            id,
            chain: Chain::Challenge,
            timestamp_unix: timestamp,
            work_started_unix: timestamp - 1,
            ips,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn final_flush_deadline_includes_waiting_for_an_old_writer() {
        let path = std::env::temp_dir().join(format!(
            "tl-history-flush-bound-{}.json",
            std::process::id()
        ));
        let state = RuntimeState::default();
        let progress = crate::operational::State::fixture().progress.clone();
        let held = progress
            .arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint)
            .try_acquire_owned()
            .unwrap();
        let store = Arc::new(Store::new(path.clone(), &Config::default(), &[]).unwrap());
        let started = tokio::time::Instant::now();
        assert!(
            store
                .flush(store.capture(&state, 1, 2), progress)
                .await
                .is_err()
        );
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert!(
            !path.exists(),
            "timed-out waiter must not launch another writer"
        );
        drop(held);
    }
    #[tokio::test]
    async fn old_optional_writer_fences_history_for_replacement_stores() {
        let dir = std::env::temp_dir().join(format!("tl-history-job-bound-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config = Config::default();
        let state = RuntimeState::default();
        let progress = crate::operational::State::fixture().progress.clone();
        let old = Arc::new(Store::new(dir.join("history.json"), &config, &[]).unwrap());
        let replacement = Arc::new(Store::new(dir.join("history.json"), &config, &[]).unwrap());
        let permit = progress
            .arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint)
            .try_acquire_owned()
            .unwrap();
        let (release, waiting) = std::sync::mpsc::channel();
        let writing = old.clone();
        let saved = old.capture(&state, 1, 2);
        let job = crate::optional_io::spawn("history-bound-fixture", move || {
            let _permit = permit;
            let _ = waiting.recv();
            writing.save(&saved)
        })
        .unwrap();
        assert!(
            replacement
                .checkpoint(replacement.capture(&state, 3, 4), progress.clone())
                .is_err()
        );
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        replacement
            .checkpoint(replacement.capture(&state, 3, 4), progress)
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let mut restored = RuntimeState::default();
        replacement.restore(&mut restored, 5).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn restart_restores_records_and_gaps_without_restoring_live_health_or_counts() {
        let (directory, store) = fixture();
        let mut first = RuntimeState::default();
        first.observations = vec![proof(1, 1002, 1_000_000.), proof(2, 1045, 400_000.)].into();
        first.estimated_observations = vec![proof(3, 1045, 425_000.)].into();
        first.events.push(Event {
            id: 5,
            timestamp_unix: 1040,
            board_id: Some("a".into()),
            kind: "recovery".into(),
            end_unix: None,
        });
        first.events.push(Event {
            id: 6,
            timestamp_unix: 1048,
            board_id: None,
            kind: "node_unavailable".into(),
            end_unix: None,
        });
        store
            .save(&store.capture(&first, 1000, 1050).pruned(1050))
            .unwrap();
        let mut second = RuntimeState::default();
        store.restore(&mut second, 1100).unwrap();
        assert!(second.observations.is_empty());
        assert_eq!(second.proofs_delivered, 0);
        assert_eq!(second.failed_proofs, 0);
        assert_eq!(second.phase, "setup_required");
        assert!(second.boards.is_empty());
        assert_eq!(second.observations_created, 3);
        assert!(second.estimated_observations.is_empty());
        assert_eq!(second.events_created, 7);
        assert_eq!(second.events[0].kind, "telemetry_gap");
        assert_eq!(second.events[0].timestamp_unix, 1050);
        assert_eq!(second.events[0].end_unix, Some(1100));
        second
            .observations
            .push(proof(second.observations_created + 1, 1110, 900_000.));
        let saved = store.capture(&second, 1100, 1120).pruned(1120);
        assert_eq!(
            saved.observations.iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        assert_eq!(saved.observations[1].ips, 400_000.);
        assert_eq!(saved.estimated_observations[0].ips, 425_000.);
        assert_eq!(saved.events[1].end_unix, Some(1050));
        assert_eq!(saved.capture_intervals.len(), 2);
        assert_eq!(saved.capture_intervals[0].end_unix, 1050);
        assert_eq!(saved.capture_intervals[1].start_unix, 1100);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn expired_invalid_and_future_observations_are_not_reloaded() {
        let (directory, store) = fixture();
        let mut state = RuntimeState::default();
        state.observations = vec![
            proof(1, 100, 1.),
            proof(2, 99_999, 700_000.),
            proof(3, 100_001, 900_000.),
            proof(4, 99_999, f64::NAN),
        ]
        .into();
        state.events.push(Event {
            id: 1,
            timestamp_unix: 100,
            board_id: None,
            kind: "recovery".into(),
            end_unix: None,
        });
        let saved = store.capture(&state, 10, 100_000).pruned(100_000);
        assert_eq!(saved.observations.len(), 1);
        assert_eq!(saved.observations[0].id, 2);
        assert!(saved.events.is_empty());
        assert_eq!(saved.capture_intervals[0].start_unix, 100_000 - DAY);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn node_or_network_change_does_not_misattribute_old_history() {
        let (directory, store) = fixture();
        let mut first = RuntimeState::default();
        first.observations.push(proof(1, 1002, 900_000.));
        store
            .save(&store.capture(&first, 1000, 1050).pruned(1050))
            .unwrap();
        let mut config = Config::default();
        config.node.host = "other-node".into();
        let other =
            Store::new(store.path.clone(), &config, &config.assignments().unwrap()).unwrap();
        let mut state = RuntimeState::default();
        other.restore(&mut state, 1100).unwrap();
        assert!(state.archive.is_none());
        config.node.host = "localhost".into();
        config.node.network = timelord_core::Network::Testnet11;
        Store::new(store.path.clone(), &config, &config.assignments().unwrap())
            .unwrap()
            .restore(&mut state, 1100)
            .unwrap();
        assert!(state.archive.is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn custom_genesis_or_constants_changes_start_a_new_history_scope() {
        let mut config = Config {
            node: crate::network::tests::node(),
            ..Default::default()
        };
        let original = scope(&config, &[]).unwrap();
        let definition = config.node.network_definition.as_mut().unwrap();
        definition.exported_at = "another export".into();
        definition.chia_version = "another version".into();
        definition
            .config_overrides
            .insert("address_prefix".into(), "another-prefix".to_owned().into());
        assert_eq!(scope(&config, &[]).unwrap(), original);
        config
            .node
            .network_definition
            .as_mut()
            .unwrap()
            .constants
            .insert("GENESIS_CHALLENGE".into(), "cd".repeat(32).into());
        assert_ne!(scope(&config, &[]).unwrap(), original);
        config.node = crate::network::tests::node();
        config
            .node
            .network_definition
            .as_mut()
            .unwrap()
            .constants
            .insert("SUB_SLOT_ITERS_STARTING".into(), 67108864.into());
        assert_ne!(scope(&config, &[]).unwrap(), original);
    }

    #[test]
    fn history_scope_uses_the_resolved_plan_instead_of_recomputing_config_order() {
        let (directory, _) = fixture();
        let mut config = Config::default();
        config.hardware.boards = (0..3)
            .map(|i| {
                toml::from_str(&format!(
                    "id='b{i}'\nname='B{i}'\nclient_port={}",
                    18110 + i
                ))
                .unwrap()
            })
            .collect();
        let configured = config.assignments().unwrap();
        let mut resolved = configured.clone();
        resolved[0].board_id = "b2".into();
        resolved[2].board_id = "b0".into();
        let path = directory.join("ranked.json");
        let store = Store::new(path.clone(), &config, &resolved).unwrap();
        let mut state = RuntimeState::default();
        state.observations.push(proof(1, 1002, 900_000.));
        store
            .save(&store.capture(&state, 1000, 1050).pruned(1050))
            .unwrap();
        let mut restored = RuntimeState::default();
        Store::new(path.clone(), &config, &configured)
            .unwrap()
            .restore(&mut restored, 1100)
            .unwrap();
        assert!(restored.archive.is_none());
        Store::new(path, &config, &resolved)
            .unwrap()
            .restore(&mut restored, 1100)
            .unwrap();
        assert!(restored.archive.is_some());
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn failed_write_and_corrupt_read_preserve_the_existing_file() {
        let (directory, store) = fixture();
        let saved = store
            .capture(&RuntimeState::default(), 1000, 1050)
            .pruned(1050);
        store.save(&saved).unwrap();
        let before = fs::read(&store.path).unwrap();
        let temporary = timelord_supervisor::atomic::pending(&store.path).unwrap();
        fs::create_dir(temporary).unwrap();
        assert!(store.save(&saved).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), before);
        fs::write(&store.path, b"truncated {").unwrap();
        let mut state = RuntimeState::default();
        assert!(store.restore(&mut state, 1100).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), b"truncated {");
        assert!(state.archive.is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }
}

//! Bounded, atomic chart checkpoints. Restored records never become live health.
use crate::{
    Shared,
    runtime::{Event, Observation, RuntimeState},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use timelord_core::Config;
use tokio::sync::watch;

const DAY: u64 = 86400;
const MAX_RECORDS: usize = 86400;
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
    pub observations: Vec<Observation>,
    // Older checkpoints contain delivery timings only; never reinterpret them
    // as exporter-compatible measurements.
    #[serde(default)]
    pub estimated_observations: Vec<Observation>,
    pub events: Vec<Event>,
    pub capture_intervals: Vec<Capture>,
}

pub struct Store {
    path: PathBuf,
    scope: String,
}

impl Store {
    pub fn new(path: PathBuf, config: &Config) -> Result<Self> {
        // Do not attribute old chain measurements to a different node or board
        // assignment. Clock/profile/binary updates intentionally preserve charts.
        let scope = serde_json::to_vec(&(
            config.node.network,
            &config.node.host,
            config.node.port,
            config.assignments().map_err(anyhow::Error::msg)?,
        ))?;
        let scope = dg_xch_core::utils::hash_256(scope)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Self { path, scope })
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
            .chain(&saved.estimated_observations)
            .map(|o| o.id)
            .max()
            .unwrap_or(0);
        state.events_created = saved.events.iter().map(|e| e.id).max().unwrap_or(0);
        if let Some(end) = saved.capture_intervals.last().map(|c| c.end_unix) {
            for event in &mut saved.events {
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

    pub fn snapshot(&self, state: &RuntimeState, started: u64, now: u64) -> Snapshot {
        let mut saved = state.archive.clone().unwrap_or_else(|| Snapshot {
            schema_version: 1,
            scope: self.scope.clone(),
            observations: vec![],
            estimated_observations: vec![],
            events: vec![],
            capture_intervals: vec![],
        });
        saved
            .observations
            .extend(state.observations.iter().cloned());
        saved
            .estimated_observations
            .extend(state.estimated_observations.iter().cloned());
        saved.events.extend(state.events.iter().cloned());
        if now > started {
            saved.capture_intervals.push(Capture {
                start_unix: started,
                end_unix: now,
            });
        }
        saved.prune(now);
        saved
    }

    pub fn save(&self, saved: &Snapshot) -> Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = self
            .path
            .with_extension(format!("history-{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            let bytes = serde_json::to_vec(saved)?;
            ensure!(
                bytes.len() as u64 <= MAX_BYTES,
                "History checkpoint is too large"
            );
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl Snapshot {
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
            observations.drain(..observations.len().saturating_sub(MAX_RECORDS));
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
        self.events
            .drain(..self.events.len().saturating_sub(MAX_RECORDS));
        self.capture_intervals
            .drain(..self.capture_intervals.len().saturating_sub(MAX_RECORDS));
    }
}

pub async fn record(
    shared: Arc<Shared>,
    store: Arc<Store>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut timer = tokio::time::interval(Duration::from_secs(5));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = timer.tick() => {},
        };
        let snapshot = store.snapshot(
            &shared.runtime.lock().expect("runtime"),
            shared.started_unix,
            crate::unix_now(),
        );
        let target = store.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || target.save(&snapshot)).await? {
            tracing::warn!(%error, "Chart history could not be saved");
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
        let store = Store::new(directory.join("config.history.json"), &Config::default()).unwrap();
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

    #[test]
    fn restart_restores_records_and_gaps_without_restoring_live_health_or_counts() {
        let (directory, store) = fixture();
        let mut first = RuntimeState {
            observations: vec![proof(1, 1002, 1_000_000.), proof(2, 1045, 400_000.)],
            estimated_observations: vec![proof(3, 1045, 425_000.)],
            ..Default::default()
        };
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
        store.save(&store.snapshot(&first, 1000, 1050)).unwrap();
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
        let saved = store.snapshot(&second, 1100, 1120);
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
        let mut state = RuntimeState {
            observations: vec![
                proof(1, 100, 1.),
                proof(2, 99_999, 700_000.),
                proof(3, 100_001, 900_000.),
                proof(4, 99_999, f64::NAN),
            ],
            ..Default::default()
        };
        state.events.push(Event {
            id: 1,
            timestamp_unix: 100,
            board_id: None,
            kind: "recovery".into(),
            end_unix: None,
        });
        let saved = store.snapshot(&state, 10, 100_000);
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
        store.save(&store.snapshot(&first, 1000, 1050)).unwrap();
        let mut config = Config::default();
        config.node.host = "other-node".into();
        let other = Store::new(store.path.clone(), &config).unwrap();
        let mut state = RuntimeState::default();
        other.restore(&mut state, 1100).unwrap();
        assert!(state.archive.is_none());
        config.node.host = "localhost".into();
        config.node.network = timelord_core::Network::Testnet11;
        Store::new(store.path.clone(), &config)
            .unwrap()
            .restore(&mut state, 1100)
            .unwrap();
        assert!(state.archive.is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_write_and_corrupt_read_preserve_the_existing_file() {
        let (directory, store) = fixture();
        let saved = store.snapshot(&RuntimeState::default(), 1000, 1050);
        store.save(&saved).unwrap();
        let before = fs::read(&store.path).unwrap();
        let temporary = store
            .path
            .with_extension(format!("history-{}.tmp", std::process::id()));
        fs::write(temporary, "interrupted temporary file").unwrap();
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

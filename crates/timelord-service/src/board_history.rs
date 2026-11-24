//! Best-effort per-board event history that survives restarts. It explains a
//! board's state (failures and causes, probation, cautious starts) but never
//! decides it, and its writes never block proof or control work.
use crate::hardware::health::Health;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

const PER_BOARD: usize = 64;
const MAX_BOARDS: usize = 64;
const MAX_BYTES: u64 = 1024 * 1024;
const DETAIL_LIMIT: usize = 240;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub timestamp_unix: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probation_until_unix: Option<u64>,
}
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    schema_version: u32,
    boards: BTreeMap<String, VecDeque<Event>>,
}

#[derive(Default)]
pub struct Store {
    path: OnceLock<PathBuf>,
    boards: Mutex<BTreeMap<String, VecDeque<Event>>>,
    writing: std::sync::Arc<AtomicBool>,
    dirty: AtomicBool,
}
impl Store {
    /// Attach the on-disk file once per process and load prior events.
    pub fn attach(&self, path: PathBuf) {
        if self.path.set(path.clone()).is_err() {
            return;
        }
        let loaded = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file() && m.len() <= MAX_BYTES)
            .and_then(|_| std::fs::read(&path).ok())
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .filter(|saved| saved.schema_version == 1);
        if let Some(saved) = loaded {
            let mut boards = self.boards.lock().expect("board history");
            for (serial, mut events) in saved.boards.into_iter().take(MAX_BOARDS) {
                events.truncate(PER_BOARD);
                boards.entry(serial).or_insert(events);
            }
        }
    }
    pub fn events(&self, serial: &str) -> Vec<Event> {
        self.boards
            .lock()
            .expect("board history")
            .get(serial)
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn push(&self, serial: &str, event: Event) {
        {
            let mut boards = self.boards.lock().expect("board history");
            if !boards.contains_key(serial) && boards.len() >= MAX_BOARDS {
                return;
            }
            let events = boards.entry(serial.to_owned()).or_default();
            if events.len() == PER_BOARD {
                events.pop_front();
            }
            events.push_back(event);
        }
        self.dirty.store(true, Ordering::Release);
        self.flush();
    }
    /// One detached writer at a time; a write that races new events leaves the
    /// dirty flag set for the next push.
    fn flush(&self) {
        let Some(path) = self.path.get().cloned() else {
            return;
        };
        if self.writing.swap(true, Ordering::AcqRel) {
            return;
        }
        self.dirty.store(false, Ordering::Release);
        let saved = Saved {
            schema_version: 1,
            boards: self.boards.lock().expect("board history").clone(),
        };
        let bytes = match serde_json::to_vec(&saved) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.writing.store(false, Ordering::Release);
                return;
            }
        };
        let writing = self.writing.clone();
        if crate::optional_io::spawn("board-history", move || {
            let result = timelord_supervisor::atomic::write(&path, &bytes);
            writing.store(false, Ordering::Release);
            if let Err(error) = &result {
                tracing::debug!(error=%format!("{error:#}"), "Board history checkpoint unavailable");
            }
            result
        })
        .is_err()
        {
            self.writing.store(false, Ordering::Release);
        }
    }
}

/// Record a board event in memory and best-effort on disk.
pub fn record(
    shared: &crate::Shared,
    board: &timelord_core::BoardConfig,
    kind: &str,
    detail: Option<&str>,
    health: Option<&Health>,
) {
    let Some(serial) = &board.serial else {
        return;
    };
    shared.process.board_history.push(
        serial,
        Event {
            timestamp_unix: crate::unix_now(),
            kind: kind.into(),
            detail: detail.map(|d| d.chars().take(DETAIL_LIMIT).collect()),
            consecutive_failures: health.map(|h| h.consecutive_failures),
            probation_until_unix: health
                .map(|h| h.probation_until_unix)
                .filter(|&until| until > 0),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(t: u64) -> Event {
        Event {
            timestamp_unix: t,
            kind: "failure".into(),
            detail: Some("x".into()),
            consecutive_failures: Some(1),
            probation_until_unix: None,
        }
    }
    #[test]
    fn history_is_bounded_and_survives_reattach() {
        let dir = std::env::temp_dir().join(format!("tl-board-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("board-history.json");
        let _ = std::fs::remove_file(&path);
        let store = Store::default();
        store.attach(path.clone());
        for t in 0..100 {
            store.push("A", event(t));
        }
        assert_eq!(store.events("A").len(), PER_BOARD);
        assert_eq!(store.events("A")[0].timestamp_unix, 36);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while store.writing.load(Ordering::Acquire) || store.dirty.load(Ordering::Acquire) {
            store.flush();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let reloaded = Store::default();
        reloaded.attach(path);
        assert_eq!(reloaded.events("A"), store.events("A"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

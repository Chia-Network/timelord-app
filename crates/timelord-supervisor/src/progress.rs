//! Bounded work snapshots. No wall clock, proof bytes, board names or credentials.
use serde::{Deserialize, Serialize};
pub const SLOTS: usize = 21;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Resource ownership is typed internally; only this module knows wire-slot positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofLane {
    Challenge,
    Reward,
    InfusedChallenge,
}
impl ProofLane {
    fn index(self) -> usize {
        match self {
            Self::Challenge => 0,
            Self::Reward => 1,
            Self::InfusedChallenge => 2,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardSlot {
    First,
    Second,
    Third,
}
impl BoardSlot {
    pub fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::First),
            1 => Some(Self::Second),
            2 => Some(Self::Third),
            _ => None,
        }
    }
    fn index(self) -> usize {
        match self {
            Self::First => 0,
            Self::Second => 1,
            Self::Third => 2,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    ProofIo(ProofLane),
    Scheduler,
    Arithmetic(ProofLane),
    Eligibility,
    Topology,
    Persistence,
    AdmissionIo(ProofLane),
    Admission,
    AdmissionArithmetic(ProofLane),
    BoardPreparation(BoardSlot),
    OptionalCheckpoint,
}
impl Resource {
    fn index(self) -> usize {
        match self {
            Self::ProofIo(chain) => chain.index(),
            Self::Scheduler => 3,
            Self::Arithmetic(chain) => 4 + chain.index(),
            Self::Eligibility => 7,
            Self::Topology => 8,
            Self::Persistence => 9,
            Self::AdmissionIo(chain) => 10 + chain.index(),
            Self::Admission => 13,
            Self::AdmissionArithmetic(chain) => 14 + chain.index(),
            Self::BoardPreparation(board) => 17 + board.index(),
            Self::OptionalCheckpoint => 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Preparing,
    WaitingForClient,
    Handshake,
    NativeWork,
    Verifying,
    Scheduler,
    Submitting,
    Eligibility { wait_seconds: u64 },
    Discovery,
    TopologyChange,
    Persistence,
    OptionalPersistence,
    Admission,
    BoardPreparation,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Work {
    pub id: u64,
    pub generation: u64,
    pub iterations: u64,
    pub stage: Stage,
}
impl Work {
    fn allowance(self) -> Option<Duration> {
        let seconds = match self.stage {
            Stage::WaitingForClient | Stage::OptionalPersistence => return None, // External capacity outage; native recovery owns it.
            Stage::Preparing => 30,
            Stage::Discovery => 15,
            Stage::Persistence => 10,
            Stage::Admission => 45,
            Stage::BoardPreparation => 60,
            Stage::TopologyChange => 60,
            Stage::Eligibility { wait_seconds } => wait_seconds.saturating_add(60),
            Stage::Handshake | Stage::Submitting => 15,
            Stage::Verifying | Stage::Scheduler => 60,
            // Conservative full target, not a held raw-speed sample. Startup,
            // checkpoint construction and normal recoveries retain 60s slack.
            Stage::NativeWork => self.iterations.div_ceil(100_000).saturating_add(60),
        };
        Some(Duration::from_secs(seconds))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub configuration_fallback: bool,
    #[serde(default)]
    pub configuration_checkpointed: bool,
    pub worker_pid: u32,
    pub generation: u64,
    pub sequence: u64,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub ready: bool,
    /// Production I/O/math, scheduler/eligibility, discovery/persistence, isolated probe I/O/math.
    pub work: [Option<Work>; SLOTS],
}
pub struct Progress {
    configuration_fallback: AtomicBool,
    configuration_checkpointed: AtomicBool,
    next: AtomicU64,
    failed: AtomicBool,
    ready: AtomicBool,
    work: Mutex<[Option<Work>; SLOTS]>,
    arithmetic: [Arc<tokio::sync::Semaphore>; SLOTS],
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            configuration_fallback: AtomicBool::new(false),
            configuration_checkpointed: AtomicBool::new(false),
            next: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            work: Mutex::new([None; SLOTS]),
            arithmetic: std::array::from_fn(|_| Arc::new(tokio::sync::Semaphore::new(1))),
        }
    }
}
impl Progress {
    pub fn using_last_good(&self) -> bool {
        self.configuration_fallback.load(Ordering::Relaxed)
    }
    pub fn configuration(&self, fallback: bool) {
        self.configuration_fallback
            .store(fallback, Ordering::Relaxed);
    }
    pub fn checkpointed(&self) {
        self.configuration_checkpointed
            .store(true, Ordering::Relaxed);
    }
    pub fn arithmetic(&self, resource: Resource) -> Arc<tokio::sync::Semaphore> {
        self.arithmetic[resource.index()].clone()
    }
    pub fn fail(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }
    pub fn begin(
        self: &Arc<Self>,
        resource: Resource,
        generation: u64,
        iterations: u64,
        stage: Stage,
    ) -> Guard {
        let lane = resource.index();
        let id = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        self.work.lock().expect("progress")[lane] = Some(Work {
            id,
            generation,
            iterations,
            stage,
        });
        Guard {
            progress: self.clone(),
            lane,
            id,
        }
    }
    pub fn snapshot(&self, generation: u64, sequence: u64) -> Option<Snapshot> {
        let work = *self.work.try_lock().ok()?;
        Some(Snapshot {
            configuration_fallback: self.configuration_fallback.load(Ordering::Relaxed),
            configuration_checkpointed: self.configuration_checkpointed.load(Ordering::Relaxed),
            worker_pid: std::process::id(),
            generation,
            sequence,
            failed: self.failed.load(Ordering::Relaxed),
            ready: self.ready.load(Ordering::Relaxed),
            work,
        })
    }
}
pub struct Guard {
    progress: Arc<Progress>,
    lane: usize,
    id: u64,
}
impl Guard {
    pub fn stage(&self, stage: Stage) {
        if let Some(work) = &mut self.progress.work.lock().expect("progress")[self.lane]
            && work.id == self.id
        {
            work.stage = stage;
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut work = self.progress.work.lock().expect("progress");
        if work[self.lane].is_some_and(|w| w.id == self.id) {
            work[self.lane] = None;
        }
    }
}

pub struct Watchdog {
    started: Instant,
    last: Option<Instant>,
    sequence: Option<u64>,
    failed: bool,
    stages: [Option<(Work, Instant)>; SLOTS],
}
impl Watchdog {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            last: None,
            sequence: None,
            failed: false,
            stages: [None; SLOTS],
        }
    }
    pub fn observe(&mut self, snapshot: Snapshot, now: Instant) -> bool {
        if self.sequence.is_some_and(|last| snapshot.sequence <= last) {
            return false;
        }
        self.failed |= snapshot.failed;
        self.sequence = Some(snapshot.sequence);
        self.last = Some(now);
        for (current, next) in self.stages.iter_mut().zip(snapshot.work) {
            *current = next.map(|work| {
                let since = current
                    .filter(|(old, _)| {
                        old.id == work.id
                            && old.generation == work.generation
                            && old.stage == work.stage
                            && old.iterations == work.iterations
                    })
                    .map(|(_, since)| since)
                    .unwrap_or(now);
                (work, since)
            });
        }
        true
    }
    pub fn age(&self, now: Instant) -> Option<u64> {
        self.last
            .map(|last| now.saturating_duration_since(last).as_secs())
    }
    pub fn failure(&self, now: Instant) -> Option<String> {
        if self.failed {
            return Some("Worker reported an essential task failure".into());
        }
        if self.last.is_none()
            && now.saturating_duration_since(self.started) > Duration::from_secs(120)
        {
            return Some("Worker did not report startup progress within 120 seconds".into());
        }
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) > Duration::from_secs(30))
        {
            return Some("Worker progress heartbeat expired".into());
        }
        for (lane, work) in self.stages.iter().enumerate() {
            if let Some((work, since)) = work
                && work
                    .allowance()
                    .is_some_and(|limit| now.saturating_duration_since(*since) > limit)
            {
                return Some(format!(
                    "Work deadline exceeded: lane={lane} request={} generation={} stage={:?} target={}",
                    work.id, work.generation, work.stage, work.iterations
                ));
            }
        }
        None
    }
}

/// Dropping a worker's reporter aborts only telemetry, never production work.
/// It runs on the worker runtime, while its deadline is checked by another process.
pub struct Reporter(tokio::task::JoinHandle<()>);
impl Reporter {
    pub fn from_environment(progress: Arc<Progress>) -> anyhow::Result<Option<Self>> {
        Self::with_readiness(progress, || false)
    }
    pub fn with_readiness(
        progress: Arc<Progress>,
        readiness: impl Fn() -> bool + Send + 'static,
    ) -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os("TIMELORD_SUPERVISOR_PROGRESS") else {
            return Ok(None);
        };
        let generation: u64 = std::env::var("TIMELORD_SUPERVISOR_GENERATION")?.parse()?;
        let socket = tokio::net::UnixDatagram::unbound()?;
        let path = std::path::PathBuf::from(path);
        Ok(Some(Self(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut sequence = 0u64;
            loop {
                interval.tick().await;
                sequence = sequence.wrapping_add(1);
                progress.ready.store(readiness(), Ordering::Relaxed);
                if let Some(snapshot) = progress.snapshot(generation, sequence)
                    && let Ok(bytes) = serde_json::to_vec(&snapshot)
                {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(250),
                        socket.send_to(&bytes, &path),
                    )
                    .await;
                }
            }
        }))))
    }
}
impl Drop for Reporter {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(sequence: u64, stage: Stage, iterations: u64) -> Snapshot {
        let mut work = [None; SLOTS];
        work[0] = Some(Work {
            id: 1,
            generation: 1,
            iterations,
            stage,
        });
        Snapshot {
            worker_pid: 1,
            generation: 1,
            sequence,
            failed: false,
            ready: false,
            configuration_fallback: false,
            configuration_checkpointed: false,
            work,
        }
    }
    #[test]
    fn long_native_work_idle_and_external_capacity_are_not_fixed_proof_timeouts() {
        let now = Instant::now();
        let mut watch = Watchdog::new(now);
        watch.observe(snapshot(1, Stage::NativeWork, 1_000_000_000), now);
        let later = now + Duration::from_secs(3600);
        watch.observe(snapshot(2, Stage::NativeWork, 1_000_000_000), later);
        assert!(watch.failure(later).is_none());
        watch.observe(snapshot(3, Stage::WaitingForClient, 1), later);
        watch.observe(
            snapshot(4, Stage::WaitingForClient, 1),
            later + Duration::from_secs(86400),
        );
        assert!(watch.failure(later + Duration::from_secs(86400)).is_none());
    }
    #[test]
    fn bounded_optional_io_stays_visible_without_restarting_useful_work() {
        let now = Instant::now();
        let mut watch = Watchdog::new(now);
        watch.observe(snapshot(1, Stage::OptionalPersistence, 0), now);
        watch.observe(
            snapshot(2, Stage::OptionalPersistence, 0),
            now + Duration::from_secs(86400),
        );
        assert!(watch.failure(now + Duration::from_secs(86400)).is_none());
        let report = Snapshot {
            worker_pid: u32::MAX,
            generation: u64::MAX,
            sequence: u64::MAX,
            failed: false,
            ready: false,
            configuration_fallback: true,
            configuration_checkpointed: true,
            work: [Some(Work {
                id: u64::MAX,
                generation: u64::MAX,
                iterations: u64::MAX,
                stage: Stage::Eligibility {
                    wait_seconds: u64::MAX,
                },
            }); SLOTS],
        };
        assert!(serde_json::to_vec(&report).unwrap().len() < 8192);
    }
    #[test]
    fn healthy_heartbeat_cannot_hide_stuck_verification_and_replays_do_not_extend_liveness() {
        let now = Instant::now();
        let mut watch = Watchdog::new(now);
        watch.observe(snapshot(1, Stage::Verifying, 1), now);
        watch.observe(
            snapshot(2, Stage::Verifying, 1),
            now + Duration::from_secs(61),
        );
        assert!(
            watch
                .failure(now + Duration::from_secs(61))
                .unwrap()
                .contains("Verifying")
        );
        watch.observe(
            snapshot(2, Stage::WaitingForClient, 1),
            now + Duration::from_secs(90),
        );
        assert!(
            watch
                .failure(now + Duration::from_secs(92))
                .unwrap()
                .contains("heartbeat")
        );
    }
    #[test]
    fn eligibility_uses_the_actual_gate_and_discovery_has_a_bounded_stage() {
        let now = Instant::now();
        let mut watch = Watchdog::new(now);
        watch.observe(
            snapshot(1, Stage::Eligibility { wait_seconds: 300 }, 1),
            now,
        );
        watch.observe(
            snapshot(2, Stage::Eligibility { wait_seconds: 300 }, 1),
            now + Duration::from_secs(301),
        );
        assert!(watch.failure(now + Duration::from_secs(301)).is_none());
        watch.observe(
            snapshot(3, Stage::Eligibility { wait_seconds: 300 }, 1),
            now + Duration::from_secs(361),
        );
        assert!(
            watch
                .failure(now + Duration::from_secs(361))
                .unwrap()
                .contains("Eligibility")
        );
        watch.observe(
            snapshot(4, Stage::Discovery, 0),
            now + Duration::from_secs(362),
        );
        watch.observe(
            snapshot(5, Stage::Discovery, 0),
            now + Duration::from_secs(378),
        );
        assert!(
            watch
                .failure(now + Duration::from_secs(378))
                .unwrap()
                .contains("Discovery")
        );
    }
    #[test]
    fn cancellation_and_replacement_cannot_clear_each_others_stage() {
        let progress = Arc::new(Progress::default());
        let old = progress.begin(
            Resource::ProofIo(ProofLane::Challenge),
            1,
            1,
            Stage::Verifying,
        );
        let new = progress.begin(
            Resource::ProofIo(ProofLane::Challenge),
            2,
            20,
            Stage::Preparing,
        );
        old.stage(Stage::NativeWork);
        drop(old);
        assert_eq!(
            progress.snapshot(1, 1).unwrap().work[0].unwrap().stage,
            Stage::Preparing
        );
        drop(new);
        assert!(
            progress
                .snapshot(1, 2)
                .unwrap()
                .work
                .iter()
                .all(Option::is_none)
        );
    }
    #[test]
    fn typed_resources_preserve_wire_slots_and_stage_changes_keep_request_identity() {
        let progress = Arc::new(Progress::default());
        let resources = [
            Resource::ProofIo(ProofLane::Challenge),
            Resource::ProofIo(ProofLane::Reward),
            Resource::ProofIo(ProofLane::InfusedChallenge),
            Resource::Scheduler,
            Resource::Arithmetic(ProofLane::Challenge),
            Resource::Arithmetic(ProofLane::Reward),
            Resource::Arithmetic(ProofLane::InfusedChallenge),
            Resource::Eligibility,
            Resource::Topology,
            Resource::Persistence,
            Resource::AdmissionIo(ProofLane::Challenge),
            Resource::AdmissionIo(ProofLane::Reward),
            Resource::AdmissionIo(ProofLane::InfusedChallenge),
            Resource::Admission,
            Resource::AdmissionArithmetic(ProofLane::Challenge),
            Resource::AdmissionArithmetic(ProofLane::Reward),
            Resource::AdmissionArithmetic(ProofLane::InfusedChallenge),
            Resource::BoardPreparation(BoardSlot::First),
            Resource::BoardPreparation(BoardSlot::Second),
            Resource::BoardPreparation(BoardSlot::Third),
            Resource::OptionalCheckpoint,
        ];
        let guards: Vec<_> = resources
            .into_iter()
            .map(|resource| progress.begin(resource, 7, 42, Stage::Preparing))
            .collect();
        let before = progress.snapshot(9, 1).unwrap();
        for (slot, work) in before.work.iter().enumerate() {
            assert_eq!(work.unwrap().id, slot as u64 + 1);
        }
        guards[3].stage(Stage::Submitting);
        let after = progress.snapshot(9, 2).unwrap();
        let work = after.work[3].unwrap();
        assert_eq!(work.id, before.work[3].unwrap().id);
        assert_eq!(work.generation, 7);
        assert_eq!(work.iterations, 42);
        assert_eq!(work.stage, Stage::Submitting);
        // Resource enums remain an internal API; the bounded wire contract stays positional.
        let wire = serde_json::to_value(&after).unwrap();
        assert_eq!(wire["work"].as_array().unwrap().len(), SLOTS);
        assert_eq!(wire["work"][3]["stage"], "submitting");
        drop(guards);
        assert!(
            progress
                .snapshot(9, 3)
                .unwrap()
                .work
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn missing_startup_and_runtime_heartbeats_trigger_replacement() {
        let now = Instant::now();
        let mut watch = Watchdog::new(now);
        assert!(watch.failure(now + Duration::from_secs(121)).is_some());
        watch.observe(
            Snapshot {
                worker_pid: 1,
                generation: 1,
                sequence: 1,
                failed: false,
                ready: false,
                configuration_fallback: false,
                configuration_checkpointed: false,
                work: [None; SLOTS],
            },
            now,
        );
        assert!(watch.failure(now + Duration::from_secs(29)).is_none());
        assert!(watch.failure(now + Duration::from_secs(31)).is_some());
    }
}

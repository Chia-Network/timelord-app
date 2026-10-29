//! Bounded work snapshots. No wall clock, proof bytes, board names or credentials.
use serde::{Deserialize, Serialize};
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
pub enum Resource {
    ProofIo(ProofLane),
    Scheduler,
    Arithmetic(ProofLane),
    Eligibility,
    Topology,
}
impl Resource {
    fn index(self) -> usize {
        match self {
            Self::ProofIo(chain) => chain.index(),
            Self::Scheduler => 3,
            Self::Arithmetic(chain) => 4 + chain.index(),
            Self::Eligibility => 7,
            Self::Topology => 8,
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
            Stage::WaitingForClient => return None, // External capacity outage; native recovery owns it.
            Stage::Preparing => 30,
            Stage::Discovery => 15,
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
    pub worker_pid: u32,
    pub generation: u64,
    pub sequence: u64,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub ready: bool,
    /// CC/RC/ICC I/O, scheduler, CC/RC/ICC arithmetic, eligibility, discovery/topology.
    pub work: [Option<Work>; 9],
}
#[derive(Default)]
pub struct Progress {
    next: AtomicU64,
    failed: AtomicBool,
    ready: AtomicBool,
    work: Mutex<[Option<Work>; 9]>,
}
impl Progress {
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
    stages: [Option<(Work, Instant)>; 9],
}
impl Watchdog {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            last: None,
            sequence: None,
            failed: false,
            stages: [None; 9],
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
        Snapshot {
            worker_pid: 1,
            generation: 1,
            sequence,
            failed: false,
            ready: false,
            work: [
                Some(Work {
                    id: 1,
                    generation: 1,
                    iterations,
                    stage,
                }),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ],
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
        assert_eq!(wire["work"].as_array().unwrap().len(), 9);
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
                work: [None; 9],
            },
            now,
        );
        assert!(watch.failure(now + Duration::from_secs(29)).is_none());
        assert!(watch.failure(now + Duration::from_secs(31)).is_some());
    }
}

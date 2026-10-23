//! Online policy. All USB writes are acknowledged by the native client's owner thread.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};
use timelord_core::{AutomaticConfig, Config};

pub fn fingerprint(config: &Config) -> Result<String> {
    let bytes = std::fs::read(
        config
            .hardware
            .client_binary
            .as_ref()
            .context("Choose a native client")?,
    )?;
    Ok(dg_xch_core::utils::hash_256(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
pub fn host() -> String {
    format!(
        "{}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.model"])
            .output()
            .ok()
            .map(|r| String::from_utf8_lossy(&r.stdout).trim().to_string())
            .unwrap_or_default()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub frequency_mhz: f64,
    pub voltage_v: f64,
}
#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub state: String,
    pub message: String,
    pub active_seconds: u64,
    pub cooldown_seconds: u64,
}
pub enum Action {
    Set(Point, String),
    Save,
}
pub struct Input {
    pub healthy: bool,
    pub recoveries: u64,
    pub score: Option<f64>,
    pub temperature: Option<f64>,
    pub temperature_limit: f64,
    pub paused: bool,
}
// A severe burst is independent of proof throughput: a storm can starve proofs.
// Keep only the last 20 events, and allow five seconds after a clock write for
// settling and pre-write log backlog. Normal recoveries still use the IPS policy.
const STORM_RECOVERIES: usize = 20;
const STORM_WINDOW_SECONDS: u64 = 10;
const SETTLING_SECONDS: u64 = 5;
#[derive(Default)]
struct RecoveryBurst {
    events: VecDeque<u64>,
    settled_at: u64,
}
impl RecoveryBurst {
    fn reset(&mut self, now: u64) {
        self.events.clear();
        self.settled_at = now.saturating_add(SETTLING_SECONDS);
    }
    fn observe(&mut self, now: u64, count: u64) -> bool {
        if now < self.settled_at {
            self.events.clear();
            return false;
        }
        while self
            .events
            .front()
            .is_some_and(|&t| now.saturating_sub(t) >= STORM_WINDOW_SECONDS)
        {
            self.events.pop_front();
        }
        for _ in 0..count.min(STORM_RECOVERIES as u64) {
            if self.events.len() == STORM_RECOVERIES {
                self.events.pop_front();
            }
            self.events.push_back(now);
        }
        self.events.len() == STORM_RECOVERIES
    }
}
pub struct Policy {
    pub point: Point,
    pub status: Status,
    pub epoch: u64,
    pub settings: AutomaticConfig,
    best: Point,
    baseline: Option<f64>,
    trial: bool,
    last_tick: u64,
    recoveries: u64,
    recovery_burst: RecoveryBurst,
    window_started: u64,
    window_recoveries: u64,
    degraded_windows: u32,
    ceiling: f64,
    was_paused: bool,
    frequencies: Vec<f64>,
}
impl Policy {
    pub fn new(settings: AutomaticConfig, point: Point, now: u64, frequencies: Vec<f64>) -> Self {
        Self {
            ceiling: settings.max_frequency_mhz,
            settings,
            point,
            best: point,
            baseline: None,
            trial: false,
            epoch: now,
            last_tick: now,
            recoveries: 0,
            recovery_burst: RecoveryBurst {
                settled_at: now.saturating_add(SETTLING_SECONDS),
                ..Default::default()
            },
            window_started: now,
            window_recoveries: 0,
            degraded_windows: 0,
            was_paused: false,
            frequencies,
            status: Status {
                state: "learning".into(),
                message: "Observing delivered proof performance".into(),
                ..Default::default()
            },
        }
    }
    pub fn restore(&mut self, entry: &Learned) {
        if entry.ceiling_mhz.is_finite() {
            self.ceiling = entry
                .ceiling_mhz
                .min(self.settings.max_frequency_mhz)
                .max(self.point.frequency_mhz);
        }
        self.status.cooldown_seconds = entry.cooldown_seconds.min(self.settings.cooldown_seconds);
    }
    pub fn in_trial(&self) -> bool {
        self.trial
    }
    pub fn learned_limits(&self) -> (f64, u64) {
        (self.ceiling, self.status.cooldown_seconds)
    }
    pub fn synchronize_recoveries(&mut self, count: u64) {
        self.recoveries = count;
    }
    pub fn applied(&mut self, point: Point, now: u64, reason: String) {
        self.point = point;
        self.recovery_burst.reset(now);
        self.epoch = now;
        self.last_tick = now;
        self.window_started = now;
        self.window_recoveries = 0;
        self.degraded_windows = 0;
        self.status.active_seconds = 0;
        self.status.state = if self.trial { "testing" } else { "learning" }.into();
        self.status.message = reason;
    }
    fn backoff(&mut self, reason: &str, force_frequency: bool) -> Option<Action> {
        let target = if !force_frequency && self.point.voltage_v < 1.0 {
            Point {
                voltage_v: 1.0,
                ..self.point
            }
        } else if self.trial && self.best.frequency_mhz < self.point.frequency_mhz {
            self.best
        } else {
            let frequency_mhz = self
                .frequencies
                .iter()
                .copied()
                .rev()
                .find(|&f| {
                    f <= self.point.frequency_mhz - 20. && f >= self.settings.min_frequency_mhz
                })
                .or_else(|| {
                    self.frequencies.iter().copied().rev().find(|&f| {
                        f < self.point.frequency_mhz && f >= self.settings.min_frequency_mhz
                    })
                })
                .unwrap_or(self.point.frequency_mhz);
            Point {
                frequency_mhz,
                ..self.point
            }
        };
        self.trial = false;
        self.degraded_windows = 0;
        self.window_recoveries = 0;
        self.status.cooldown_seconds = self.settings.cooldown_seconds;
        if target == self.point {
            self.status.state = "monitoring".into();
            self.status.message = format!("{reason}; already at the minimum frequency");
            return None;
        }
        if target.frequency_mhz < self.point.frequency_mhz {
            self.ceiling = self.point.frequency_mhz - 10.;
        }
        self.baseline = None;
        Some(Action::Set(target, reason.into()))
    }
    pub fn observe(&mut self, now: u64, input: Input) -> Option<Action> {
        let clock_reversed = now < self.last_tick;
        let dt = now.saturating_sub(self.last_tick).min(5);
        self.last_tick = now;
        let reset_counter = input.recoveries < self.recoveries;
        let mut new_recoveries = input.recoveries.saturating_sub(self.recoveries);
        if reset_counter {
            self.recovery_burst.reset(now);
        }
        self.window_recoveries = self.window_recoveries.saturating_add(new_recoveries);
        self.recoveries = input.recoveries;
        if now < self.epoch || clock_reversed {
            self.recovery_burst.reset(now);
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.baseline = None;
            return None;
        }
        if input.paused {
            self.recovery_burst.reset(now);
            self.status.state = "paused".into();
            self.status.message = "Current settings held; proof production continues".into();
            self.was_paused = true;
            self.trial = false;
            self.best = self.point;
            return None;
        }
        if self.was_paused {
            self.recovery_burst.reset(now);
            new_recoveries = 0;
            self.was_paused = false;
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.baseline = None;
        }
        if self.recovery_burst.observe(now, new_recoveries) {
            self.recovery_burst.reset(now);
            return self.backoff("Recovery storm: 20 restarts within 10 seconds", true);
        }
        if !input.healthy {
            self.status.state = "waiting".into();
            self.status.message = "Waiting for fresh proof work and temperature telemetry".into();
            return None;
        }
        // Soft thermal backoff supplements the independent hard temperature stop.
        if input
            .temperature
            .is_some_and(|t| t >= input.temperature_limit - 1.)
        {
            return self.backoff("Reduced frequency near the temperature limit", true);
        }
        self.status.active_seconds += dt;
        self.status.cooldown_seconds = self.status.cooldown_seconds.saturating_sub(dt);
        self.status.state = if self.trial { "testing" } else { "learning" }.into();
        let score = input.score.filter(|s| s.is_finite() && *s > 0.)?;
        if let Some(baseline) = self.baseline
            && now.saturating_sub(self.window_started) >= self.settings.recovery_window_seconds
        {
            let degraded = self.window_recoveries > 0
                && score < baseline * (1. - self.settings.recovery_loss_percent / 100.);
            self.degraded_windows = if degraded {
                self.degraded_windows + 1
            } else {
                0
            };
            self.window_started = now;
            self.window_recoveries = 0;
            if self.degraded_windows >= self.settings.recovery_bad_windows {
                return self.backoff("Sustained delivered IPS loss with recoveries in consecutive observation windows",false);
            }
        }
        if self.status.active_seconds < self.settings.healthy_seconds {
            return None;
        }
        if self.degraded_windows > 0 {
            self.status.message = "Waiting for a complete recovery-cost observation window".into();
            return None;
        }
        if self.trial {
            // No rollback merely because an otherwise stable trial missed a tiny gain.
            // A materially slower recovering trial stays under observation until the
            // consecutive-window test above can establish a problem.
            if self
                .baseline
                .is_some_and(|b| score < b * (1. - self.settings.recovery_loss_percent / 100.))
                && self.window_recoveries > 0
            {
                self.status.message =
                    "Observing recovery cost before accepting this setting".into();
                return None;
            }
            self.trial = false;
            self.best = self.point;
            self.baseline = Some(score);
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.status.message =
                "Retained higher frequency; continuing to monitor delivered IPS".into();
            return Some(Action::Save);
        }
        if self.baseline.is_none() {
            self.best = self.point;
            self.baseline = Some(score);
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.message =
                "Saved observed performance baseline; recoveries are permitted".into();
            return Some(Action::Save);
        }
        if self.status.cooldown_seconds > 0 {
            self.status.state = "cooldown".into();
            return None;
        }
        if input
            .temperature
            .is_none_or(|t| t >= input.temperature_limit - 3.)
        {
            self.status.state = "monitoring".into();
            return None;
        }
        if let Some(frequency_mhz) = self
            .frequencies
            .iter()
            .copied()
            .find(|&f| f >= self.point.frequency_mhz + 10. - 0.000001 && f <= self.ceiling)
        {
            self.best = self.point;
            self.trial = true;
            return Some(Action::Set(
                Point {
                    frequency_mhz,
                    ..self.point
                },
                format!("Testing supported frequency {frequency_mhz:.0} MHz"),
            ));
        }
        self.status.state = "monitoring".into();
        self.status.message =
            "Holding frequency and voltage; monitoring delivered IPS and recovery cost".into();
        None
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Learned {
    pub serial: String,
    pub engines: usize,
    // Missing in older profiles, which used the lowest-numbered engines.
    #[serde(default)]
    pub engine_mask: Option<u8>,
    pub host: String,
    pub client_sha256: String,
    pub point: Point,
    pub active_seconds: u64,
    pub updated_unix: u64,
    pub ceiling_mhz: f64,
    pub cooldown_seconds: u64,
}
impl Learned {
    fn physical_mask(&self) -> Option<u8> {
        self.engine_mask.or_else(|| match self.engines {
            1..=3 => Some((1 << self.engines) - 1),
            _ => None,
        })
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    schema_version: u32,
    entries: Vec<Learned>,
}
fn cache(path: &Path) -> Result<Cache> {
    if !path.exists() {
        return Ok(Cache {
            schema_version: 2,
            ..Default::default()
        });
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        bail!("Invalid Automatic state file");
    }
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if value["schema_version"] == 1 {
        // Preserve observed operating points while discarding zero-recovery
        // ceilings and obsolete voltage-search state from the previous policy.
        if let Some(entries) = value["entries"].as_array_mut() {
            for entry in entries {
                if let Some(entry) = entry.as_object_mut() {
                    entry.remove("voltage_rejected");
                    entry.insert("ceiling_mhz".into(), 1600.into());
                    entry.insert("cooldown_seconds".into(), 0.into());
                }
            }
        }
        value["schema_version"] = 2.into();
    }
    let value: Cache = serde_json::from_value(value)?;
    if value.schema_version != 2 {
        bail!("Unsupported Automatic state version");
    }
    Ok(value)
}
pub fn learned(
    config: &Config,
    board: &timelord_core::BoardConfig,
    engines: usize,
    hash: &str,
    host: &str,
) -> Result<Option<Learned>> {
    let Some(path) = config.hardware.automatic_state.as_ref() else {
        return Ok(None);
    };
    Ok(cache(path)?
        .entries
        .into_iter()
        .filter(|e| {
            Some(&e.serial) == board.serial.as_ref()
                && e.engines == engines
                && e.physical_mask() == timelord_core::engine_mask(engines)
                && e.client_sha256 == hash
                && e.host == host
                && board
                    .automatic
                    .contains(e.point.frequency_mhz, e.point.voltage_v)
        })
        .max_by_key(|e| e.updated_unix))
}
pub fn initial(
    config: &Config,
    board: &timelord_core::BoardConfig,
    engines: usize,
    hash: &str,
    host: &str,
) -> Result<Point> {
    Ok(learned(config, board, engines, hash, host)?
        .map(|e| Point {
            voltage_v: e.point.voltage_v.max(board.automatic.start_voltage_v),
            ..e.point
        })
        .unwrap_or(Point {
            frequency_mhz: board.automatic.start_frequency_mhz,
            voltage_v: board.automatic.start_voltage_v,
        }))
}
pub fn save(path: &Path, entry: Learned) -> Result<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Automatic state lock failed"))?;
    let mut value = cache(path)?;
    value.entries.retain(|e| {
        !(e.serial == entry.serial
            && e.engines == entry.engines
            && e.physical_mask() == entry.physical_mask()
            && e.host == entry.host
            && e.client_sha256 == entry.client_sha256)
    });
    value.entries.push(entry);
    if value.entries.len() > 128 {
        value.entries.drain(..value.entries.len() - 128);
    }
    let parent = path.parent().context("Automatic state folder missing")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".automatic-{}-{}.tmp",
        std::process::id(),
        crate::unix_now()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&value)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub fn phase_score(
    observations: &[crate::runtime::Observation],
    chains: &[timelord_core::Chain],
    epoch: u64,
    minimum: usize,
) -> Option<f64> {
    let mut score = f64::INFINITY;
    for chain in chains {
        let values: Vec<_> = observations
            .iter()
            .rev()
            .take_while(|o| o.timestamp_unix >= epoch)
            .filter(|o| {
                o.chain == *chain
                    && o.work_started_unix >= epoch.saturating_add(10)
                    && o.ips.is_finite()
                    && o.ips > 0.
            })
            .map(|o| o.ips)
            .collect();
        if values.len() < minimum {
            return None;
        }
        score = score.min(values.iter().sum::<f64>() / values.len() as f64);
    }
    score.is_finite().then_some(score)
}

pub fn socket_path(config_path: &Path, config: &Config) -> PathBuf {
    config.service.control_socket.clone().unwrap_or_else(|| {
        let canonical = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.to_owned());
        let digest = dg_xch_core::utils::hash_256(canonical.as_os_str().as_encoded_bytes());
        let suffix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        // macOS limits Unix socket paths to 104 bytes. Long Chia/config roots
        // therefore use a stable, same-user private temporary socket path.
        std::env::temp_dir().join(format!("tl-{}-{suffix}.sock", unsafe { libc::geteuid() }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frequencies() -> Vec<f64> {
        (20..=160).map(|i| f64::from(i) * 10.).collect()
    }
    fn settings() -> AutomaticConfig {
        AutomaticConfig {
            healthy_seconds: 30,
            cooldown_seconds: 30,
            recovery_window_seconds: 60,
            ..Default::default()
        }
    }
    fn input() -> Input {
        Input {
            healthy: true,
            recoveries: 0,
            score: Some(950000.),
            temperature: Some(40.),
            temperature_limit: 55.,
            paused: false,
        }
    }
    fn policy() -> Policy {
        Policy::new(
            settings(),
            Point {
                frequency_mhz: 1450.,
                voltage_v: 1.,
            },
            0,
            frequencies(),
        )
    }
    #[test]
    fn sparse_pll_steps_skip_unsupported_requests_and_respect_soft_ceiling() {
        let point = Point {
            frequency_mhz: 1500.,
            voltage_v: 1.,
        };
        let supported = vec![1480., 1490., 1500., 1520., 1540.];
        let mut p = Policy::new(settings(), point, 0, supported.clone());
        let mut next = None;
        for now in 1..=40 {
            if let Some(Action::Set(target, _)) = p.observe(now, input()) {
                next = Some(target);
                break;
            }
        }
        assert_eq!(next.unwrap().frequency_mhz, 1520.);
        p.applied(next.unwrap(), 41, "trial".into());
        let mut recovery = input();
        recovery.recoveries = 1;
        assert!(
            p.observe(42, recovery).is_none(),
            "One recovery must not reject a supported trial"
        );
        assert_eq!(p.point.frequency_mhz, 1520.);
        let mut config = settings();
        config.max_frequency_mhz = 1510.;
        let mut limited = Policy::new(config, point, 0, supported.clone());
        for now in 1..=100 {
            assert!(!matches!(
                limited.observe(now, input()),
                Some(Action::Set(..))
            ));
        }
        let mut backoff = Policy::new(
            settings(),
            Point {
                frequency_mhz: 1540.,
                voltage_v: 1.,
            },
            0,
            supported,
        );
        let mut thermal = input();
        thermal.temperature = Some(54.);
        let Some(Action::Set(target, _)) = backoff.observe(1, thermal) else {
            panic!("missing thermal backoff")
        };
        assert_eq!(target.frequency_mhz, 1520.);
    }

    #[test]
    fn idle_and_unattributed_proofs_cannot_promote_a_setting() {
        let mut p = policy();
        for now in 0..300 {
            let mut i = input();
            i.healthy = false;
            assert!(p.observe(now, i).is_none());
        }
        assert_eq!(p.status.active_seconds, 0);
        for now in 300..500 {
            let mut i = input();
            i.score = None;
            assert!(p.observe(now, i).is_none());
        }
        assert_eq!(p.point.frequency_mhz, 1450.);
        use timelord_core::Chain;
        let observations: Vec<_> = (0..3)
            .flat_map(|_| [Chain::Challenge, Chain::Reward])
            .enumerate()
            .map(|(n, chain)| crate::runtime::Observation {
                id: n as u64,
                chain,
                timestamp_unix: 200 + n as u64,
                work_started_unix: 50,
                ips: 950000.,
            })
            .collect();
        assert_eq!(
            phase_score(&observations, &[Chain::Challenge, Chain::Reward], 100, 3),
            None
        );
        let fresh: Vec<_> = observations
            .into_iter()
            .map(|mut o| {
                o.work_started_unix = 120;
                o.ips = if o.chain == Chain::Reward {
                    900000.
                } else {
                    1000000.
                };
                o
            })
            .collect();
        assert_eq!(
            phase_score(&fresh, &[Chain::Challenge, Chain::Reward], 100, 3),
            Some(900000.)
        );
        assert_eq!(
            phase_score(
                &fresh,
                &[Chain::Challenge, Chain::Reward, Chain::InfusedChallenge],
                100,
                3
            ),
            None
        );
    }
    fn raised_trial() -> Policy {
        let mut p = policy();
        for now in (5..=35).step_by(5) {
            if let Some(Action::Set(point, reason)) = p.observe(now, input()) {
                p.applied(point, now, reason);
            }
        }
        assert_eq!(p.point.frequency_mhz, 1460.);
        p
    }

    #[test]
    fn storm_rejects_a_trial_without_a_score_or_fresh_proofs() {
        let mut p = raised_trial();
        let mut sample = input();
        sample.healthy = false;
        sample.score = None;
        sample.recoveries = 19;
        assert!(p.observe(40, sample).is_none());
        let mut storm = input();
        storm.healthy = false;
        storm.score = None;
        storm.recoveries = 20;
        let Some(Action::Set(target, reason)) = p.observe(40, storm) else {
            panic!("storm must not wait for a score");
        };
        assert_eq!(target.frequency_mhz, 1450.);
        assert!(reason.contains("Recovery storm"));
        assert_eq!(p.learned_limits(), (1450., 30));
        p.applied(target, 40, reason);
        p.synchronize_recoveries(20);
        let mut settled = input();
        settled.recoveries = 20;
        settled.score = None;
        assert!(
            p.observe(45, settled).is_none(),
            "Do not count the previous point twice"
        );
    }

    #[test]
    fn recorded_1920_burst_backs_off_in_five_seconds() {
        // First six one-second bins from ASIC 2's October 22 20:34 CDT trial.
        let counts = [14, 3, 2, 5, 5, 82];
        let point = Point {
            frequency_mhz: 1920.,
            voltage_v: 1.,
        };
        let mut p = Policy::new(
            AutomaticConfig::default(),
            point,
            0,
            vec![1880., 1900., 1920., 1940.],
        );
        let mut total = 0;
        for (now, count) in counts.into_iter().enumerate() {
            for _ in 0..count {
                total += 1;
                let mut sample = input();
                sample.healthy = false;
                sample.score = None;
                sample.recoveries = total;
                if let Some(Action::Set(target, _)) = p.observe(now as u64, sample) {
                    assert_eq!(now, 5);
                    assert_eq!(
                        target,
                        Point {
                            frequency_mhz: 1900.,
                            voltage_v: 1.
                        }
                    );
                    assert_eq!(total, 49);
                    assert!(p.recovery_burst.events.len() <= STORM_RECOVERIES);
                    return;
                }
            }
        }
        panic!("recorded storm was not rejected");
    }

    #[test]
    fn rolling_storm_window_expires_and_memory_is_bounded() {
        let mut p = policy();
        let mut sample = input();
        sample.score = None;
        sample.recoveries = 19;
        assert!(p.observe(5, sample).is_none());
        let mut next = input();
        next.score = None;
        next.recoveries = 20;
        assert!(
            p.observe(15, next).is_none(),
            "Recoveries ten seconds apart are not a burst"
        );
        let mut burst = RecoveryBurst::default();
        assert!(burst.observe(10, u64::MAX));
        assert_eq!(burst.events.len(), STORM_RECOVERIES);
        assert!(!burst.observe(20, 0));
    }

    #[test]
    fn pause_counter_reset_and_clock_reversal_do_not_create_storms() {
        let mut p = policy();
        let mut paused = input();
        paused.paused = true;
        paused.recoveries = 100;
        assert!(p.observe(5, paused).is_none());
        let mut resumed = input();
        resumed.recoveries = 200;
        resumed.score = None;
        assert!(p.observe(6, resumed).is_none());
        let mut unchanged = input();
        unchanged.recoveries = 200;
        unchanged.score = None;
        assert!(p.observe(11, unchanged).is_none());
        let mut reset = input();
        reset.score = None;
        reset.recoveries = 0;
        assert!(p.observe(12, reset).is_none());
        let mut nineteen = input();
        nineteen.score = None;
        nineteen.recoveries = 19;
        assert!(p.observe(20, nineteen).is_none());
        let mut backwards = input();
        backwards.score = None;
        backwards.recoveries = 20;
        assert!(p.observe(19, backwards).is_none());
        let mut later = input();
        later.score = None;
        later.recoveries = 20;
        assert!(p.observe(24, later).is_none());
    }

    #[test]
    fn storm_forces_lower_frequency_even_at_lower_voltage_and_during_cooldown() {
        let mut p = policy();
        p.point.voltage_v = 0.95;
        p.status.cooldown_seconds = 30;
        let mut sample = input();
        sample.recoveries = 20;
        sample.score = None;
        let Some(Action::Set(target, reason)) = p.observe(5, sample) else {
            panic!("missing fast backoff");
        };
        assert!(target.frequency_mhz < p.point.frequency_mhz);
        assert_eq!(target.voltage_v, 0.95);
        p.applied(target, 5, reason);
        p.synchronize_recoveries(20);
        let mut old_logs = input();
        old_logs.recoveries = 150;
        old_logs.score = None;
        assert!(p.observe(6, old_logs).is_none());
        let mut no_new_errors = input();
        no_new_errors.recoveries = 150;
        no_new_errors.score = None;
        assert!(
            p.observe(10, no_new_errors).is_none(),
            "Old-point log backlog must not reject the new point"
        );
        let mut continued = input();
        continued.recoveries = 170;
        continued.score = None;
        let Some(Action::Set(lower, _)) = p.observe(10, continued) else {
            panic!("cooldown must not block protective backoff");
        };
        assert!(lower.frequency_mhz < target.frequency_mhz);
    }

    #[test]
    fn storm_at_frequency_floor_does_not_repeat_a_hardware_write() {
        let mut p = policy();
        p.settings.min_frequency_mhz = p.point.frequency_mhz;
        let mut storm = input();
        storm.recoveries = 20;
        storm.score = None;
        assert!(p.observe(5, storm).is_none());
        assert!(p.status.message.contains("minimum frequency"));
    }

    #[test]
    fn moderately_bursty_recoveries_with_good_performance_remain_allowed() {
        let mut p = policy();
        p.settings.max_frequency_mhz = p.point.frequency_mhz;
        p.ceiling = p.point.frequency_mhz;
        for now in (5..=3600).step_by(5) {
            let mut sample = input();
            sample.recoveries = now / 10 * 3;
            assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
        }
    }

    #[test]
    fn normal_recoveries_with_good_ips_do_not_reduce_frequency_or_voltage_over_hours() {
        let mut config = settings();
        config.max_frequency_mhz = 1450.;
        let mut p = Policy::new(config, policy().point, 0, frequencies());
        for now in (5..=14400).step_by(5) {
            let mut sample = input();
            sample.recoveries = now / 50;
            assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
            assert_eq!(
                p.point,
                Point {
                    frequency_mhz: 1450.,
                    voltage_v: 1.
                }
            );
        }
        assert_eq!(p.status.state, "monitoring");
    }

    #[test]
    fn recovered_faster_trial_is_accepted_but_two_degraded_windows_back_off() {
        let mut faster = raised_trial();
        for now in (40..=65).step_by(5) {
            let mut sample = input();
            sample.recoveries = 2;
            sample.score = Some(970000.);
            assert!(!matches!(
                faster.observe(now, sample),
                Some(Action::Set(..))
            ));
        }
        assert!(!faster.in_trial());
        assert_eq!(faster.point.frequency_mhz, 1460.);
        let mut p = raised_trial();
        for now in (40..=150).step_by(5) {
            let mut sample = input();
            sample.recoveries = 1 + (now - 40) / 60;
            sample.score = Some(850000.);
            assert!(
                !matches!(p.observe(now, sample), Some(Action::Set(..))),
                "premature backoff at {now}"
            );
        }
        let mut sample = input();
        sample.recoveries = 2;
        sample.score = Some(850000.);
        let Some(Action::Set(target, reason)) = p.observe(155, sample) else {
            panic!("missing recovery-cost backoff")
        };
        assert_eq!(
            target,
            Point {
                frequency_mhz: 1450.,
                voltage_v: 1.
            }
        );
        assert!(reason.contains("consecutive"));
        assert_eq!(p.status.cooldown_seconds, 30);
    }

    #[test]
    fn one_bad_window_then_recovery_keeps_the_higher_setting() {
        let mut p = raised_trial();
        p.ceiling = 1460.;
        for now in (40..=95).step_by(5) {
            let mut sample = input();
            sample.recoveries = 1;
            sample.score = Some(850000.);
            assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
        }
        assert_eq!(p.degraded_windows, 1);
        for now in (100..=150).step_by(5) {
            let mut good = input();
            good.recoveries = 1;
            good.score = Some(970000.);
            assert!(p.observe(now, good).is_none());
        }
        let mut good = input();
        good.recoveries = 1;
        good.score = Some(970000.);
        assert!(matches!(p.observe(155, good), Some(Action::Save)));
        assert_eq!(p.degraded_windows, 0);
        assert_eq!(p.point.frequency_mhz, 1460.);
    }

    #[test]
    fn flat_performance_and_pause_do_not_restore_a_lower_setting() {
        let mut p = raised_trial();
        for now in (40..=65).step_by(5) {
            assert!(!matches!(p.observe(now, input()), Some(Action::Set(..))));
        }
        assert!(!p.in_trial());
        assert_eq!(p.point.frequency_mhz, 1460.);
        let mut p = raised_trial();
        let mut paused = input();
        paused.paused = true;
        assert!(p.observe(40, paused).is_none());
        assert_eq!(p.point.frequency_mhz, 1460.);
        for now in (45..=100).step_by(5) {
            let mut sample = input();
            sample.paused = true;
            assert!(p.observe(now, sample).is_none());
        }
        assert!(p.observe(105, input()).is_none());
        assert_eq!(p.status.active_seconds, 5);
    }

    #[test]
    fn missing_gain_without_recoveries_is_not_a_hardware_failure_and_heat_still_backs_off() {
        let mut p = raised_trial();
        p.ceiling = 1460.;
        for now in (40..=400).step_by(5) {
            let mut sample = input();
            sample.score = Some(800000.);
            assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
        }
        assert_eq!(p.point.frequency_mhz, 1460.);
        let mut thermal = input();
        thermal.temperature = Some(54.);
        let Some(Action::Set(target, _)) = p.observe(405, thermal) else {
            panic!("thermal backoff missing")
        };
        assert!(target.frequency_mhz < p.point.frequency_mhz);
        assert_eq!(target.voltage_v, 1.);
        let mut warm = policy();
        for now in (5..=120).step_by(5) {
            let mut sample = input();
            sample.temperature = Some(53.);
            assert!(!matches!(warm.observe(now, sample), Some(Action::Set(..))));
        }
    }

    #[test]
    fn sustained_recovery_loss_at_lower_voltage_raises_voltage_before_lowering_frequency() {
        let mut config = settings();
        config.max_frequency_mhz = 1450.;
        let mut p = Policy::new(
            config,
            Point {
                frequency_mhz: 1450.,
                voltage_v: 0.95,
            },
            0,
            frequencies(),
        );
        for now in (5..=30).step_by(5) {
            p.observe(now, input());
        }
        for now in (35..=145).step_by(5) {
            let mut sample = input();
            sample.recoveries = 1 + (now - 35) / 60;
            sample.score = Some(850000.);
            assert!(!matches!(p.observe(now, sample), Some(Action::Set(..))));
        }
        let mut sample = input();
        sample.recoveries = 2;
        sample.score = Some(850000.);
        let Some(Action::Set(target, _)) = p.observe(150, sample) else {
            panic!("missing voltage restoration")
        };
        assert_eq!(
            target,
            Point {
                frequency_mhz: 1450.,
                voltage_v: 1.
            }
        );
    }

    #[test]
    fn old_profiles_keep_the_observed_frequency_without_old_recovery_or_voltage_search_limits() {
        let path =
            std::env::temp_dir().join(format!("tl-profile-migration-{}.json", std::process::id()));
        let old = serde_json::json!({"schema_version":1,"entries":[{
            "serial":"a","engines":2,"host":"mac","client_sha256":"hash",
            "point":{"frequency_mhz":1500.,"voltage_v":0.95},"active_seconds":120,
            "updated_unix":1,"ceiling_mhz":1500.,"cooldown_seconds":900,"voltage_rejected":true}]});
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut config = Config::default();
        config.hardware.automatic_state = Some(path.clone());
        let board: timelord_core::BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\nserial='a'").unwrap();
        let point = initial(&config, &board, 2, "hash", "mac").unwrap();
        assert_eq!(
            point,
            Point {
                frequency_mhz: 1500.,
                voltage_v: 1.
            }
        );
        let migrated = cache(&path).unwrap();
        assert_eq!(migrated.schema_version, 2);
        assert_eq!(migrated.entries[0].ceiling_mhz, 1600.);
        assert_eq!(migrated.entries[0].cooldown_seconds, 0);
        let mut entry = migrated.entries[0].clone();
        entry.point = point;
        save(&path, entry).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("voltage_rejected"));
        assert_eq!(cache(&path).unwrap().entries.len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn simultaneous_board_updates_keep_all_learned_profiles() {
        let path =
            std::env::temp_dir().join(format!("tl-cache-concurrent-{}.json", std::process::id()));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    save(
                        &path,
                        Learned {
                            serial: format!("board-{n}"),
                            engines: 1,
                            engine_mask: Some(2),
                            host: "mac".into(),
                            client_sha256: "hash".into(),
                            point: Point {
                                frequency_mhz: 1450.,
                                voltage_v: 1.,
                            },
                            active_seconds: 120,
                            updated_unix: n,
                            ceiling_mhz: 1600.,
                            cooldown_seconds: 0,
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(cache(&path).unwrap().entries.len(), 8);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cache_is_bound_to_serial_engine_host_and_binary() {
        let directory = std::env::temp_dir().join(format!(
            "automatic-cache-{}-{}",
            std::process::id(),
            crate::unix_now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        let mut config = Config::default();
        config.hardware.automatic_state = Some(path.clone());
        let board: timelord_core::BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\nserial='a'\nprofile='automatic'")
                .unwrap();
        let entry = Learned {
            serial: "a".into(),
            engines: 3,
            engine_mask: None,
            host: "mac".into(),
            client_sha256: "hash".into(),
            point: Point {
                frequency_mhz: 1520.,
                voltage_v: 1.,
            },
            active_seconds: 120,
            updated_unix: 1,
            ceiling_mhz: 1520.,
            cooldown_seconds: 30,
        };
        save(&path, entry.clone()).unwrap();
        assert_eq!(
            initial(&config, &board, 3, "hash", "mac")
                .unwrap()
                .frequency_mhz,
            1520.
        );
        let entry = learned(&config, &board, 3, "hash", "mac").unwrap().unwrap();
        let mut restored = Policy::new(board.automatic.clone(), entry.point, 0, frequencies());
        restored.restore(&entry);
        assert_eq!(restored.learned_limits(), (1520., 30));
        for (engines, hash, host) in [
            (1, "hash", "mac"),
            (3, "different", "mac"),
            (3, "hash", "other"),
        ] {
            assert_eq!(
                initial(&config, &board, engines, hash, host)
                    .unwrap()
                    .frequency_mhz,
                1450.
            );
        }
        // An old single-engine profile belongs to engine 0, not the new engine 1.
        let mut old = entry;
        old.engines = 1;
        old.engine_mask = None;
        save(&path, old.clone()).unwrap();
        assert!(
            learned(&config, &board, 1, "hash", "mac")
                .unwrap()
                .is_none()
        );
        let mut current = old;
        current.engine_mask = Some(2);
        current.ceiling_mhz = 1980.;
        save(&path, current.clone()).unwrap();
        let restored = learned(&config, &board, 1, "hash", "mac").unwrap().unwrap();
        assert_eq!(restored.engine_mask, Some(2));
        assert_eq!(restored.ceiling_mhz, 1980.);
        current.ceiling_mhz = 1960.;
        save(&path, current).unwrap();
        assert_eq!(cache(&path).unwrap().entries.len(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}

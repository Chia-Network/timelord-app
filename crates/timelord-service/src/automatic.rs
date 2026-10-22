//! Online policy. All USB writes are acknowledged by the native client's owner thread.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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
    pub fn applied(&mut self, point: Point, now: u64, reason: String) {
        self.point = point;
        self.epoch = now;
        self.last_tick = now;
        self.window_started = now;
        self.window_recoveries = 0;
        self.degraded_windows = 0;
        self.status.active_seconds = 0;
        self.status.state = if self.trial { "testing" } else { "learning" }.into();
        self.status.message = reason;
    }
    fn backoff(&mut self, reason: &str, thermal: bool) -> Option<Action> {
        let target = if !thermal && self.point.voltage_v < 1.0 {
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
        let dt = now.saturating_sub(self.last_tick).min(5);
        self.last_tick = now;
        self.window_recoveries = self
            .window_recoveries
            .saturating_add(input.recoveries.saturating_sub(self.recoveries));
        self.recoveries = input.recoveries;
        if now < self.epoch {
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.baseline = None;
            return None;
        }
        if input.paused {
            self.status.state = "paused".into();
            self.status.message = "Current settings held; proof production continues".into();
            self.was_paused = true;
            self.trial = false;
            self.best = self.point;
            return None;
        }
        if self.was_paused {
            self.was_paused = false;
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.baseline = None;
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
    pub host: String,
    pub client_sha256: String,
    pub point: Point,
    pub active_seconds: u64,
    pub updated_unix: u64,
    pub ceiling_mhz: f64,
    pub cooldown_seconds: u64,
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
            "serial":"a","engines":1,"host":"mac","client_sha256":"hash",
            "point":{"frequency_mhz":1500.,"voltage_v":0.95},"active_seconds":120,
            "updated_unix":1,"ceiling_mhz":1500.,"cooldown_seconds":900,"voltage_rejected":true}]});
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut config = Config::default();
        config.hardware.automatic_state = Some(path.clone());
        let board: timelord_core::BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\nserial='a'").unwrap();
        let point = initial(&config, &board, 1, "hash", "mac").unwrap();
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
        save(&path, entry).unwrap();
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

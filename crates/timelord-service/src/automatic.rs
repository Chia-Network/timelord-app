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
#[derive(Clone, Copy)]
enum Trial {
    Frequency,
    Voltage,
}
pub enum Action {
    Set(Point, String),
    Save,
    Stop(String),
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
    trial: Option<Trial>,
    last_tick: u64,
    recoveries: u64,
    frequency_active: u64,
    ceiling: f64,
    voltage_rejected: bool,
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
            epoch: now,
            last_tick: now,
            recoveries: 0,
            baseline: None,
            trial: None,
            frequency_active: 0,
            voltage_rejected: false,
            was_paused: false,
            frequencies,
            status: Status {
                state: "learning".into(),
                message: "Observing verified live proofs".into(),
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
        self.voltage_rejected = entry.voltage_rejected;
    }
    pub fn in_trial(&self) -> bool {
        self.trial.is_some()
    }
    pub fn learned_limits(&self) -> (f64, u64, bool) {
        (
            self.ceiling,
            self.status.cooldown_seconds,
            self.voltage_rejected,
        )
    }
    pub fn applied(&mut self, point: Point, now: u64, reason: String) {
        if (point.frequency_mhz - self.point.frequency_mhz).abs() > 1. {
            self.frequency_active = 0;
        }
        self.point = point;
        self.epoch = now;
        self.last_tick = now;
        self.status.active_seconds = 0;
        self.status.state = if self.trial.is_some() {
            "testing"
        } else {
            "learning"
        }
        .into();
        self.status.message = reason;
    }
    pub fn observe(&mut self, now: u64, input: Input) -> Option<Action> {
        let dt = now.saturating_sub(self.last_tick).min(5);
        self.last_tick = now;
        if now < self.epoch {
            self.epoch = now;
            self.status.active_seconds = 0;
            self.baseline = None;
            return None;
        }
        let recovery = input.recoveries > self.recoveries;
        self.recoveries = input.recoveries;
        if input.paused {
            self.status.state = "paused".into();
            self.status.message = "Automatic tuning is paused".into();
            self.was_paused = true;
            if self.trial.take().is_some() {
                return Some(Action::Set(
                    self.best,
                    "Holding the previous observed setting".into(),
                ));
            }
            return None;
        }
        if self.was_paused {
            self.was_paused = false;
            self.epoch = now;
            self.status.active_seconds = 0;
            self.baseline = None;
        }
        if recovery {
            let trial = self.trial.take();
            let target = if matches!(trial, Some(Trial::Voltage)) {
                self.voltage_rejected = true;
                self.best
            } else if trial.is_some() {
                self.ceiling = self.point.frequency_mhz - 10.;
                self.best
            } else if self.point.voltage_v < 1.0
                && input.healthy
                && input
                    .temperature
                    .is_some_and(|t| t < input.temperature_limit - 3.)
            {
                self.voltage_rejected = true;
                Point {
                    voltage_v: 1.0,
                    ..self.point
                }
            } else {
                self.ceiling = self.point.frequency_mhz - 10.;
                Point {
                    frequency_mhz: self
                        .frequencies
                        .iter()
                        .copied()
                        .rev()
                        .find(|&f| {
                            f <= self.point.frequency_mhz - 20.
                                && f >= self.settings.min_frequency_mhz
                        })
                        .unwrap_or_else(|| {
                            self.frequencies
                                .iter()
                                .copied()
                                .find(|&f| {
                                    f >= self.settings.min_frequency_mhz
                                        && f < self.point.frequency_mhz
                                })
                                .unwrap_or(self.point.frequency_mhz)
                        }),
                    voltage_v: self.point.voltage_v,
                }
            };
            self.baseline = None;
            self.status.cooldown_seconds = self.settings.cooldown_seconds;
            if target == self.point {
                return Some(Action::Stop(
                    "Recoveries continue at the minimum Automatic setting".into(),
                ));
            }
            let reason = if target.voltage_v > self.point.voltage_v {
                "Restored higher voltage after a recovery"
            } else {
                "Backed off after a recovery"
            };
            return Some(Action::Set(target, reason.into()));
        }
        if !input.healthy {
            self.status.state = "waiting".into();
            self.status.message = "Waiting for fresh proof work and temperature telemetry".into();
            return None;
        }
        self.status.active_seconds += dt;
        self.frequency_active += dt;
        self.status.cooldown_seconds = self.status.cooldown_seconds.saturating_sub(dt);
        self.status.state = if self.trial.is_some() {
            "testing"
        } else {
            "learning"
        }
        .into();
        let score = input.score.filter(|s| s.is_finite() && *s > 0.)?;
        if self.status.active_seconds < self.settings.healthy_seconds {
            return None;
        }
        if let Some(trial) = self.trial.take() {
            let baseline = self.baseline.expect("trial baseline");
            let accepted = score
                >= baseline
                    * if matches!(trial, Trial::Frequency) {
                        1.002
                    } else {
                        0.998
                    };
            if accepted {
                self.best = self.point;
                self.baseline = Some(score);
                self.epoch = now;
                self.status.active_seconds = 0;
                self.status.message = "Accepted an observed clean setting".into();
                return Some(Action::Save);
            }
            if matches!(trial, Trial::Voltage) {
                self.voltage_rejected = true;
            } else {
                self.ceiling = self.point.frequency_mhz - 10.;
            }
            self.baseline = None;
            self.status.cooldown_seconds = self.settings.cooldown_seconds;
            return Some(Action::Set(
                self.best,
                "Restored the previous setting; delivered proof performance did not improve".into(),
            ));
        }
        if self.baseline.is_none() {
            self.best = self.point;
            self.baseline = Some(score);
            self.status.message = "Saved observed clean settings".into();
            return Some(Action::Save);
        }
        if self.status.cooldown_seconds > 0 {
            self.status.state = "cooldown".into();
            return None;
        }
        // Leave thermal headroom for upward exploration; the hard guard still stops at the configured limit.
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
            self.baseline = Some(score);
            self.best = self.point;
            self.trial = Some(Trial::Frequency);
            return Some(Action::Set(
                Point {
                    frequency_mhz,
                    ..self.point
                },
                format!("Testing supported frequency {frequency_mhz:.0} MHz"),
            ));
        }
        self.status.state = "monitoring".into();
        if !self.voltage_rejected
            && self.frequency_active >= self.settings.voltage_after_seconds
            && self.point.voltage_v - 0.00625 >= self.settings.min_voltage_v
        {
            self.baseline = Some(score);
            self.best = self.point;
            self.trial = Some(Trial::Voltage);
            return Some(Action::Set(
                Point {
                    voltage_v: self.point.voltage_v - 0.00625,
                    ..self.point
                },
                "Testing a lower voltage at the settled frequency".into(),
            ));
        }
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
    pub voltage_rejected: bool,
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
            schema_version: 1,
            ..Default::default()
        });
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        bail!("Invalid Automatic state file");
    }
    let value: Cache = serde_json::from_slice(&std::fs::read(path)?)?;
    if value.schema_version != 1 {
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
        .map(|e| e.point)
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
            voltage_after_seconds: 60,
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
        let Some(Action::Set(rollback, _)) = p.observe(42, recovery) else {
            panic!("missing rollback")
        };
        assert_eq!(rollback.frequency_mhz, 1500.);
        let mut config = settings();
        config.max_frequency_mhz = 1510.;
        config.voltage_after_seconds = 1000;
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
        let mut recovery = input();
        recovery.recoveries = 1;
        let Some(Action::Set(target, _)) = backoff.observe(1, recovery) else {
            panic!("missing backoff")
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
    #[test]
    fn initial_point_backs_off_below_1450_and_trials_revert_before_cooldown() {
        let mut low_voltage = Policy::new(
            settings(),
            Point {
                frequency_mhz: 1450.,
                voltage_v: 0.95,
            },
            0,
            frequencies(),
        );
        let mut failure = input();
        failure.recoveries = 1;
        assert!(matches!(
            low_voltage.observe(1, failure),
            Some(Action::Set(
                Point {
                    frequency_mhz: 1450.,
                    voltage_v: 1.0
                },
                _
            ))
        ));
        let mut p = policy();
        let mut i = input();
        i.recoveries = 1;
        assert!(matches!(
            p.observe(1, i),
            Some(Action::Set(
                Point {
                    frequency_mhz: 1430.,
                    ..
                },
                _
            ))
        ));
        let mut p = policy();
        let mut saved = false;
        for now in (5..=35).step_by(5) {
            match p.observe(now, input()) {
                Some(Action::Save) => saved = true,
                Some(Action::Set(point, reason)) => {
                    assert!(saved);
                    assert_eq!(point.frequency_mhz, 1460.);
                    p.applied(point, now, reason)
                }
                _ => {}
            }
        }
        let mut i = input();
        i.recoveries = 1;
        let Some(Action::Set(point, reason)) = p.observe(36, i) else {
            panic!("must revert")
        };
        assert_eq!(point.frequency_mhz, 1450.);
        p.applied(point, 36, reason);
        for now in 37..60 {
            let mut i = input();
            i.recoveries = 1;
            assert!(!matches!(p.observe(now, i), Some(Action::Set(..))));
        }
    }
    #[test]
    fn voltage_trials_restore_voltage_and_pause_discards_trial_evidence() {
        let mut s = settings();
        s.max_frequency_mhz = 1450.;
        let mut p = Policy::new(s, policy().point, 0, frequencies());
        for now in (5..=60).step_by(5) {
            if let Some(Action::Set(point, reason)) = p.observe(now, input()) {
                assert_eq!(point.voltage_v, 0.99375);
                p.applied(point, now, reason);
            }
        }
        let mut i = input();
        i.recoveries = 1;
        let Some(Action::Set(point, _)) = p.observe(61, i) else {
            panic!("must restore voltage")
        };
        assert_eq!(point.voltage_v, 1.);
        assert_eq!(point.frequency_mhz, 1450.);
        let mut p = policy();
        for now in (5..=35).step_by(5) {
            if let Some(Action::Set(point, reason)) = p.observe(now, input()) {
                p.applied(point, now, reason);
            }
        }
        let mut i = input();
        i.paused = true;
        assert!(matches!(
            p.observe(36, i),
            Some(Action::Set(
                Point {
                    frequency_mhz: 1450.,
                    ..
                },
                _
            ))
        ));
        for now in 37..100 {
            let mut i = input();
            i.paused = true;
            assert!(p.observe(now, i).is_none());
        }
        assert!(p.observe(100, input()).is_none());
        assert_eq!(p.status.active_seconds, 1);
    }
    #[test]
    fn heat_and_missing_improvement_stop_upward_exploration() {
        let mut p = policy();
        for now in (5..=120).step_by(5) {
            let mut i = input();
            i.temperature = Some(53.);
            assert!(!matches!(p.observe(now, i), Some(Action::Set(..))));
        }
        let mut p = policy();
        for now in (5..=35).step_by(5) {
            if let Some(Action::Set(point, reason)) = p.observe(now, input()) {
                p.applied(point, now, reason);
            }
        }
        let mut reverted = false;
        for now in (40..=70).step_by(5) {
            let mut i = input();
            i.score = Some(800000.);
            if let Some(Action::Set(point, _)) = p.observe(now, i) {
                assert_eq!(point.frequency_mhz, 1450.);
                reverted = true;
                break;
            }
        }
        assert!(reverted);
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
                            voltage_rejected: false,
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
            voltage_rejected: true,
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
        assert_eq!(restored.learned_limits(), (1520., 30, true));
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

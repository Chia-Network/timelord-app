//! Durable per-serial board state: native-owner epochs, safety restrictions,
//! cautious starts and reliability. Only overheating and invalid proofs are
//! safety restrictions; an unconfirmed shutdown only requires a cautious start.
use crate::hardware::health::Health;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};
use timelord_supervisor::{InstanceLock, Paths};

const MAX_BYTES: u64 = 256 * 1024;
const MAX_BOARDS: usize = 64;
const SCHEMA_VERSION: u32 = 2;
/// Fault-free serving needed before a lowered recovery ceiling rises 50 MHz.
pub const CEILING_RELAX_SECONDS: u64 = 6 * 3600;
#[cfg(test)]
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Thermal,
    InvalidProof,
}
impl Reason {
    /// Restrictions only strengthen until the live owner clears them.
    pub fn merge(self, next: Self) -> Self {
        if self == Self::Thermal || next == Self::Thermal {
            Self::Thermal
        } else {
            Self::InvalidProof
        }
    }
    fn cooldown_seconds(self) -> u64 {
        match self {
            Self::Thermal => 30,
            Self::InvalidProof => 10,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyReason {
    Thermal,
    InvalidProof,
    UncleanOwner,
}

/// Immutable journal policy shared by restoration, routing and native startup.
#[derive(Clone, Debug, Default)]
pub(crate) struct Policy {
    pub restriction: Option<Reason>,
    pub cautious: bool,
    pub retry_after_unix: u64,
    pub health: Health,
}
impl Policy {
    pub fn hold_reason(&self, now: u64) -> Option<String> {
        self.restriction
            .filter(|_| self.retry_after_unix > now)
            .map(|reason| {
                let wait = self.retry_after_unix.saturating_sub(now);
                match reason {
                    Reason::Thermal => format!(
                        "Cooling down after reaching the temperature limit; next check in {wait} seconds"
                    ),
                    Reason::InvalidProof => format!(
                        "Rechecking after an invalid proof; next check in {wait} seconds"
                    ),
                }
            })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Board {
    pub owner_active: bool,
    pub owner_epoch: u64,
    pub restriction: Option<Reason>,
    pub cautious: bool,
    pub updated_unix: u64,
    pub retry_after_unix: u64,
    pub last_point: Option<crate::automatic::Point>,
    pub recovery_ceiling_mhz: Option<f64>,
    pub safety_clear_since_unix: u64,
    pub health: Health,
}
/// Reads schema 1 (quarantine/force_safe/qualification) and schema 2.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBoard {
    owner_active: bool,
    #[serde(default)]
    owner_epoch: u64,
    #[serde(default)]
    restriction: Option<Reason>,
    #[serde(default)]
    quarantine: Option<LegacyReason>,
    #[serde(default)]
    cautious: bool,
    #[serde(default)]
    force_safe: bool,
    #[serde(default)]
    #[allow(dead_code)]
    qualification: Option<serde_json::Value>,
    #[serde(default)]
    updated_unix: u64,
    #[serde(default)]
    retry_after_unix: u64,
    #[serde(default)]
    last_point: Option<crate::automatic::Point>,
    #[serde(default)]
    recovery_ceiling_mhz: Option<f64>,
    #[serde(default)]
    admission_frequency_mhz: Option<f64>,
    #[serde(default)]
    safety_clear_since_unix: u64,
    #[serde(default)]
    health: Health,
}
impl StoredBoard {
    fn upgrade(self, now: u64) -> Board {
        let (restriction, legacy_unclean) = match self.quarantine {
            Some(LegacyReason::Thermal) => (Some(Reason::Thermal), false),
            Some(LegacyReason::InvalidProof) => (Some(Reason::InvalidProof), false),
            Some(LegacyReason::UncleanOwner) => (None, true),
            None => (None, false),
        };
        let ceiling = self.recovery_ceiling_mhz.or(self.admission_frequency_mhz);
        Board {
            owner_active: self.owner_active,
            owner_epoch: self.owner_epoch,
            restriction: self.restriction.or(restriction),
            cautious: self.cautious || self.force_safe || legacy_unclean,
            updated_unix: self.updated_unix,
            retry_after_unix: self.retry_after_unix,
            last_point: self.last_point,
            recovery_ceiling_mhz: ceiling,
            safety_clear_since_unix: if self.safety_clear_since_unix == 0 && ceiling.is_some() {
                now
            } else {
                self.safety_clear_since_unix
            },
            health: self.health,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredState {
    schema_version: u32,
    boards: BTreeMap<String, StoredBoard>,
}
#[derive(Clone, Default, Serialize)]
struct Saved {
    schema_version: u32,
    boards: BTreeMap<String, Board>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerToken(pub(crate) u64);

pub struct Store {
    path: PathBuf,
    state: Mutex<Saved>,
    update_order: Mutex<()>,
    progress: Option<std::sync::Arc<timelord_supervisor::progress::Progress>>,
    /// A hardware check shares the physical facts (owner epochs, cautious
    /// starts, restrictions) but never changes reliability health: its forced
    /// settings and synthetic work say nothing about how a board serves.
    keep_health: bool,
}
fn serial_valid(serial: &str) -> bool {
    !serial.is_empty() && serial.len() <= 15 && serial.bytes().all(|b| (33..=126).contains(&b))
}
// The account database, rather than HOME/TMPDIR, defines the stable host-wide
// lease namespace. A user-owned persistent parent prevents other UIDs from
// precreating it and keeps temporary-directory cleaners away from held inodes.
fn account_home() -> Result<PathBuf> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0u8; 65536];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    ensure!(
        status == 0 && !result.is_null() && !entry.pw_dir.is_null(),
        "Cannot resolve service account home"
    );
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes(),
    ));
    let metadata = fs::symlink_metadata(&home)?;
    ensure!(
        home.is_absolute()
            && metadata.is_dir()
            && metadata.uid() == uid
            && metadata.mode() & 0o022 == 0,
        "Service account home must be owned, persistent and not writable by other users"
    );
    Ok(home)
}
fn lease_root() -> Result<PathBuf> {
    #[cfg(not(test))]
    let home = account_home()?;
    #[cfg(test)]
    let home = {
        static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            use std::os::unix::fs::DirBuilderExt;
            let root = std::env::temp_dir().join(format!(
                "tl-lease-home-{}-{}",
                std::process::id(),
                crate::unix_now()
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .expect("test lease home");
            root
        })
        .clone()
    };
    Ok(home.join(".timelord-usb-locks"))
}

fn valid_point(point: crate::automatic::Point) -> bool {
    point.frequency_mhz.is_finite()
        && (200.0..=2200.0).contains(&point.frequency_mhz)
        && point.voltage_v.is_finite()
        && (0.7..=1.0).contains(&point.voltage_v)
}

fn validate_saved(state: &Saved) -> Result<()> {
    ensure!(
        state.boards.len() <= MAX_BOARDS,
        "Oversized board safety state"
    );
    for (serial, board) in &state.boards {
        ensure!(serial_valid(serial), "Invalid saved board identity");
        ensure!(
            board
                .recovery_ceiling_mhz
                .is_none_or(|f| f.is_finite() && (200.0..=2200.0).contains(&f)),
            "Invalid saved recovery ceiling"
        );
        ensure!(
            board.last_point.is_none_or(valid_point),
            "Invalid saved operating point"
        );
        ensure!(board.health.validate(), "Invalid saved board health");
    }
    Ok(())
}

fn parse(bytes: &[u8], now: u64) -> Result<Saved> {
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Board safety state exceeds limit"
    );
    let stored: StoredState =
        serde_json::from_slice(bytes).context("Reading board safety state")?;
    ensure!(
        (1..=SCHEMA_VERSION).contains(&stored.schema_version),
        "Unsupported board safety state version"
    );
    let saved = Saved {
        schema_version: SCHEMA_VERSION,
        boards: stored
            .boards
            .into_iter()
            .map(|(serial, board)| (serial, board.upgrade(now)))
            .collect(),
    };
    validate_saved(&saved)?;
    Ok(saved)
}

impl Store {
    pub fn open(config: &Path) -> Result<Self> {
        let paths = Paths::for_config(config)?;
        let path = paths.directory.join("board-safety.json");
        let mut state = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e.into()),
            Ok(_) => {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                let m = file.metadata()?;
                ensure!(
                    m.is_file()
                        && m.uid() == unsafe { libc::geteuid() }
                        && m.nlink() == 1
                        && m.mode() & 0o077 == 0,
                    "Board safety state must be a private owned regular file"
                );
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                match parse(&bytes, crate::unix_now()) {
                    Ok(saved) => saved,
                    Err(error) => {
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_nanos();
                        let archived = path.with_file_name(format!(
                            "board-safety-corrupt-{}-{stamp}.json",
                            std::process::id()
                        ));
                        fs::rename(&path, &archived)
                            .context("Preserving invalid board recovery metadata")?;
                        tracing::warn!(%error, path=%archived.display(), "Archived invalid recovery metadata; boards will start cautiously");
                        Saved::default()
                    }
                }
            }
        };
        state.schema_version = SCHEMA_VERSION;
        for board in state.boards.values_mut() {
            if board.owner_active {
                // The prior worker did not confirm native shutdown. The board's
                // physical state is unknown, which needs a cautious start but is
                // not evidence of a safety fault.
                board.cautious = true;
                board.owner_active = false;
            }
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
            update_order: Mutex::new(()),
            progress: None,
            keep_health: false,
        })
    }
    #[cfg(test)]
    pub fn held_fixture(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        // A stalled critical journal writer: it owns the writer order while
        // its disk I/O hangs. Readers of the published state are not blocked.
        let _guard = self.update_order.lock().expect("fixture safety");
        let _ = entered.send(());
        let _ = release.recv_timeout(std::time::Duration::from_secs(2));
    }
    /// For the explicit hardware check: see `keep_health`.
    pub fn diagnostic(mut self) -> Self {
        self.keep_health = true;
        self
    }
    pub fn monitored(
        mut self,
        progress: std::sync::Arc<timelord_supervisor::progress::Progress>,
    ) -> Self {
        self.progress = Some(progress);
        self
    }
    pub fn board(&self, serial: &str) -> Option<Board> {
        self.state
            .lock()
            .expect("safety")
            .boards
            .get(serial)
            .cloned()
    }
    pub fn health(&self, serial: &str) -> Health {
        self.board(serial).map(|b| b.health).unwrap_or_default()
    }
    pub fn lease(&self, serial: &str) -> Result<InstanceLock> {
        ensure!(
            serial_valid(serial),
            "Managed ASIC requires a printable persistent serial"
        );
        let name = format!("usb-{:x}.lock", Sha256::digest(serial.as_bytes()));
        // USB ownership is shared across configurations, not just generations
        // of one worker. Keep a stable per-UID host lock namespace.
        let root = lease_root()?;
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = fs::symlink_metadata(&root)?;
        ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "ASIC ownership directory must be private and owned"
        );
        InstanceLock::acquire(&root.join(name))
    }
    fn update(&self, serial: &str, relaxing: bool, change: impl FnOnce(&mut Board)) -> Result<()> {
        self.transition(serial, relaxing, |_| true, change)
            .map(|_| ())
    }
    fn transition(
        &self,
        serial: &str,
        relaxing: bool,
        accepts: impl FnOnce(&Board) -> bool,
        change: impl FnOnce(&mut Board),
    ) -> Result<bool> {
        ensure!(serial_valid(serial), "Invalid persistent board serial");
        // Only the critical holder owns this watchdog slot. A queued writer
        // cannot replace its ID and reset a stalled transaction's deadline.
        let _order = self.update_order.lock().expect("safety writer");
        let _stage = self.progress.as_ref().map(|p| {
            p.begin(
                timelord_supervisor::progress::Resource::Persistence,
                0,
                0,
                timelord_supervisor::progress::Stage::Persistence,
            )
        });
        let mut next = self.state.lock().expect("safety").clone();
        ensure!(
            next.boards.contains_key(serial) || next.boards.len() < MAX_BOARDS,
            "Board safety registry is full"
        );
        let board = next.boards.entry(serial.into()).or_default();
        if !accepts(board) {
            return Ok(false);
        }
        change(board);
        board.updated_unix = crate::unix_now();
        if !relaxing {
            // Publish restrictions before I/O; a relaxation is published only
            // once the complete transaction is durable.
            *self.state.lock().expect("safety") = next.clone();
        }
        self.save(&next)?;
        *self.state.lock().expect("safety") = next;
        Ok(true)
    }
    pub(crate) fn policy(&self, board: &timelord_core::BoardConfig) -> Policy {
        board
            .serial
            .as_ref()
            .and_then(|serial| self.board(serial))
            .map(|saved| Policy {
                restriction: saved.restriction,
                cautious: saved.cautious,
                retry_after_unix: saved.retry_after_unix,
                health: saved.health,
            })
            // A board never seen before has unknown physical state.
            .unwrap_or(Policy {
                cautious: true,
                ..Policy::default()
            })
    }
    pub fn begin_owner(&self, serial: &str, point: crate::automatic::Point) -> Result<OwnerToken> {
        ensure!(valid_point(point), "Invalid native operating point");
        let mut token = OwnerToken(0);
        self.update(serial, false, |b| {
            b.owner_active = true;
            b.owner_epoch = b.owner_epoch.checked_add(1).expect("owner epoch exhausted");
            token = OwnerToken(b.owner_epoch);
            b.last_point = Some(point);
        })?;
        Ok(token)
    }
    /// A matching live owner established fresh reset/readback and cool
    /// telemetry and is about to resume from a low-power probe.
    pub fn prepare_recovery_point(
        &self,
        serial: &str,
        token: OwnerToken,
        point: crate::automatic::Point,
    ) -> Result<bool> {
        ensure!(valid_point(point), "Invalid recovery operating point");
        self.transition(
            serial,
            false,
            |b| b.owner_active && b.owner_epoch == token.0,
            |b| b.last_point = Some(point),
        )
    }
    /// The live owner passed its recovery checks; clear safety restrictions.
    pub fn recovered(&self, serial: &str, token: OwnerToken) -> Result<bool> {
        self.transition(
            serial,
            true,
            |b| b.owner_active && b.owner_epoch == token.0,
            |b| {
                if b.restriction.take().is_some() {
                    b.safety_clear_since_unix = crate::unix_now();
                }
                b.retry_after_unix = 0;
            },
        )
    }
    /// The live owner verified work at its cautious point and returned to its
    /// best-known setting.
    pub fn clear_cautious(&self, serial: &str, token: OwnerToken) -> Result<bool> {
        self.transition(
            serial,
            true,
            |b| b.owner_active && b.owner_epoch == token.0 && b.restriction.is_none(),
            |b| b.cautious = false,
        )
    }
    pub fn mark_cautious(&self, serial: &str) -> Result<()> {
        self.update(serial, false, |b| b.cautious = true)
    }
    pub fn owner_token(&self, serial: &str) -> Option<OwnerToken> {
        self.board(serial)
            .filter(|b| b.owner_active)
            .map(|b| OwnerToken(b.owner_epoch))
    }
    /// A point request belongs to one native session and cannot begin another.
    pub fn set_point(
        &self,
        serial: &str,
        token: OwnerToken,
        point: crate::automatic::Point,
    ) -> Result<bool> {
        ensure!(valid_point(point), "Invalid native operating point");
        self.transition(
            serial,
            false,
            |b| b.owner_active && b.owner_epoch == token.0 && b.restriction.is_none(),
            |b| b.last_point = Some(point),
        )
    }
    pub fn closed(&self, serial: &str) -> Result<()> {
        self.update(serial, true, |b| b.owner_active = false)
    }
    /// Record overheating or an invalid proof. The first fault at a point
    /// lowers the recovery ceiling 50 MHz below that point.
    pub fn restrict(&self, serial: &str, reason: Reason) -> Result<()> {
        self.update(serial, false, |b| {
            if b.restriction.is_none()
                && let Some(point) = b.last_point
            {
                let next = (point.frequency_mhz - 50.).max(200.);
                b.recovery_ceiling_mhz =
                    Some(b.recovery_ceiling_mhz.map_or(next, |old| old.min(next)));
            }
            let reason = b
                .restriction
                .map_or(reason, |previous| previous.merge(reason));
            b.restriction = Some(reason);
            b.cautious = true;
            b.safety_clear_since_unix = crate::unix_now();
            b.retry_after_unix = crate::unix_now().saturating_add(reason.cooldown_seconds());
        })
    }
    /// After sustained fault-free serving, raise a lowered recovery ceiling one
    /// step. Returns the new ceiling when it changed.
    pub fn relax_ceiling(&self, serial: &str, token: OwnerToken) -> Result<Option<Option<f64>>> {
        let now = crate::unix_now();
        let mut changed = None;
        self.transition(
            serial,
            true,
            |b| {
                b.owner_active
                    && b.owner_epoch == token.0
                    && b.restriction.is_none()
                    && b.recovery_ceiling_mhz.is_some()
                    && now.saturating_sub(b.safety_clear_since_unix) >= CEILING_RELAX_SECONDS
            },
            |b| {
                let raised = b
                    .recovery_ceiling_mhz
                    .map(|f| f + 50.)
                    .filter(|f| *f < crate::hardware::CAUTIOUS_CEILING_MHZ);
                b.recovery_ceiling_mhz = raised;
                b.safety_clear_since_unix = now;
                changed = Some(raised);
            },
        )?;
        Ok(changed)
    }
    /// Reliability updates. A failure is restrictive and published before I/O.
    pub fn record_failure(&self, serial: &str, reason: &str) -> Result<Health> {
        if self.keep_health {
            return Ok(self.health(serial));
        }
        let mut health = Health::default();
        self.update(serial, false, |b| {
            b.health.record_failure(crate::unix_now(), reason);
            health = b.health.clone();
        })?;
        Ok(health)
    }
    pub fn update_health(
        &self,
        serial: &str,
        relaxing: bool,
        change: impl FnOnce(&mut Health),
    ) -> Result<Health> {
        if self.keep_health {
            return Ok(self.health(serial));
        }
        let mut health = Health::default();
        self.update(serial, relaxing, |b| {
            change(&mut b.health);
            health = b.health.clone();
        })?;
        Ok(health)
    }
    fn save(&self, state: &Saved) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "Board safety state exceeds limit"
        );
        timelord_supervisor::atomic::write(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::Ordering;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "safety-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn config(&self) -> PathBuf {
            self.0.join("config.toml")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn point(frequency_mhz: f64) -> crate::automatic::Point {
        crate::automatic::Point {
            frequency_mhz,
            voltage_v: 1.,
        }
    }
    fn board(serial: &str) -> timelord_core::BoardConfig {
        toml::from_str(&format!(
            "id='a'\nname='A'\nserial='{serial}'\nclient_port=18110\nprofile='manual'"
        ))
        .unwrap()
    }
    #[test]
    fn unknown_and_unconfirmed_boards_start_cautiously_without_a_restriction() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        assert!(store.policy(&board("A")).cautious);
        let token = store.begin_owner("A", point(1450.)).unwrap();
        assert!(store.clear_cautious("A", token).unwrap());
        assert!(!store.policy(&board("A")).cautious);
        // The worker disappears without confirming native shutdown.
        let reopened = Store::open(&f.config()).unwrap();
        let policy = reopened.policy(&board("A"));
        assert!(policy.cautious);
        assert_eq!(policy.restriction, None);
        assert!(policy.hold_reason(0).is_none());
        assert!(!reopened.board("A").unwrap().owner_active);
    }
    #[test]
    fn schema_one_journals_upgrade_without_reviving_unclean_quarantine() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        fs::write(
            &store.path,
            br#"{"schema_version":1,"boards":{"A":{"owner_active":false,"owner_epoch":4,"quarantine":"unclean_owner","force_safe":true,"qualification":null,"updated_unix":1,"retry_after_unix":0,"last_point":{"frequency_mhz":1470.0,"voltage_v":1.0},"admission_frequency_mhz":null},"B":{"owner_active":true,"owner_epoch":9,"quarantine":"thermal","force_safe":false,"qualification":null,"updated_unix":1,"retry_after_unix":5,"last_point":null,"admission_frequency_mhz":1400.0}}}"#,
        )
        .unwrap();
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600)).unwrap();
        let upgraded = Store::open(&f.config()).unwrap();
        let a = upgraded.board("A").unwrap();
        assert_eq!(a.restriction, None);
        assert!(a.cautious);
        assert_eq!(a.owner_epoch, 4);
        let b = upgraded.board("B").unwrap();
        assert_eq!(b.restriction, Some(Reason::Thermal));
        assert_eq!(b.recovery_ceiling_mhz, Some(1400.));
        assert!(b.cautious && !b.owner_active);
        assert!(b.safety_clear_since_unix > 0);
        // A write produces schema 2 that reads back identically.
        upgraded.closed("A").unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&upgraded.path).unwrap()).unwrap();
        assert_eq!(saved["schema_version"], 2);
        assert!(saved["boards"]["A"].get("quarantine").is_none());
        assert_eq!(Store::open(&f.config()).unwrap().board("B").unwrap(), b);
    }
    #[test]
    fn thermal_wins_in_both_orders_keeps_cooldown_and_lowers_ceiling_once() {
        for order in [
            [Reason::Thermal, Reason::InvalidProof],
            [Reason::InvalidProof, Reason::Thermal],
        ] {
            let f = Fixture::new();
            let store = Store::open(&f.config()).unwrap();
            store.begin_owner("A", point(1600.)).unwrap();
            for reason in order {
                store.restrict("A", reason).unwrap();
            }
            let saved = store.board("A").unwrap();
            assert_eq!(saved.restriction, Some(Reason::Thermal));
            assert_eq!(saved.recovery_ceiling_mhz, Some(1550.));
            assert!(saved.cautious);
            assert!(
                store
                    .policy(&board("A"))
                    .hold_reason(crate::unix_now())
                    .is_some()
            );
        }
    }
    #[test]
    fn only_the_current_live_owner_clears_restrictions_and_cautious_starts() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let old = store.begin_owner("A", point(1450.)).unwrap();
        store.restrict("A", Reason::InvalidProof).unwrap();
        let current = store.begin_owner("A", point(1400.)).unwrap();
        assert!(!store.recovered("A", old).unwrap());
        assert!(
            !store.clear_cautious("A", current).unwrap(),
            "restriction first"
        );
        assert!(store.recovered("A", current).unwrap());
        assert!(store.clear_cautious("A", current).unwrap());
        let record = store.board("A").unwrap();
        assert!(record.restriction.is_none() && !record.cautious && record.owner_active);
        assert!(!store.set_point("A", old, point(1500.)).unwrap());
        assert!(store.set_point("A", current, point(1500.)).unwrap());
    }
    #[test]
    fn lowered_ceiling_relaxes_only_after_sustained_fault_free_serving() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.begin_owner("A", point(1400.)).unwrap();
        store.restrict("A", Reason::Thermal).unwrap();
        let token = store.begin_owner("A", point(1300.)).unwrap();
        store.recovered("A", token).unwrap();
        assert_eq!(store.relax_ceiling("A", token).unwrap(), None);
        // Simulate six fault-free hours.
        store
            .update("A", false, |b| {
                b.safety_clear_since_unix = crate::unix_now() - CEILING_RELAX_SECONDS
            })
            .unwrap();
        assert_eq!(store.relax_ceiling("A", token).unwrap(), Some(Some(1400.)));
        store
            .update("A", false, |b| {
                b.safety_clear_since_unix = crate::unix_now() - CEILING_RELAX_SECONDS
            })
            .unwrap();
        assert_eq!(store.relax_ceiling("A", token).unwrap(), Some(None));
        assert_eq!(store.board("A").unwrap().recovery_ceiling_mhz, None);
    }
    #[test]
    fn a_hardware_check_keeps_safety_facts_but_not_reliability_health() {
        let f = Fixture::new();
        Store::open(&f.config())
            .unwrap()
            .record_failure("A", "before")
            .unwrap();
        let check = Store::open(&f.config()).unwrap().diagnostic();
        assert_eq!(
            check
                .record_failure("A", "check")
                .unwrap()
                .consecutive_failures,
            1
        );
        check
            .update_health("A", true, |health| health.consecutive_failures = 0)
            .unwrap();
        check.restrict("A", Reason::Thermal).unwrap();
        let production = Store::open(&f.config()).unwrap();
        assert_eq!(production.health("A").consecutive_failures, 1);
        assert_eq!(
            production.board("A").unwrap().restriction,
            Some(Reason::Thermal)
        );
    }
    #[test]
    fn health_survives_reopen_and_failures_publish_before_io() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        for _ in 0..3 {
            store
                .record_failure("A", "Waiting for ASIC client timed out")
                .unwrap();
        }
        let reopened = Store::open(&f.config()).unwrap();
        let health = reopened.health("A");
        assert_eq!(health.consecutive_failures, 3);
        assert_eq!(
            health.tier(crate::unix_now()),
            crate::hardware::health::Tier::Probation
        );
        // A failed write still publishes the restrictive in-memory update.
        let directory = reopened.path.parent().unwrap().to_owned();
        let old = f.0.join("saved-directory");
        fs::rename(&directory, &old).unwrap();
        fs::write(&directory, b"not a directory").unwrap();
        assert!(reopened.record_failure("A", "again").is_err());
        assert_eq!(reopened.health("A").consecutive_failures, 4);
        assert!(reopened.restrict("A", Reason::InvalidProof).is_err());
        assert_eq!(
            reopened.board("A").unwrap().restriction,
            Some(Reason::InvalidProof)
        );
        assert!(
            reopened
                .clear_cautious("A", OwnerToken(0))
                .is_ok_and(|v| !v)
        );
        fs::remove_file(&directory).unwrap();
        fs::rename(old, &directory).unwrap();
    }
    #[test]
    fn production_lease_home_is_owned_and_outside_temporary_storage() {
        let home = account_home().unwrap();
        assert!(!home.starts_with("/tmp") && !home.starts_with("/private/tmp"));
        assert!(fs::metadata(home).unwrap().is_dir());
    }
    #[test]
    fn leases_are_exclusive_and_invalid_metadata_is_archived_for_cautious_recovery() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let serial = format!("fixture{:x}", std::process::id());
        let separate = format!("other{:x}", std::process::id());
        let other = Fixture::new();
        let second = Store::open(&other.config()).unwrap();
        let lease = store.lease(&serial).unwrap();
        assert!(store.lease(&serial).is_err());
        assert!(second.lease(&serial).is_err());
        assert!(second.lease(&separate).is_ok());
        drop(lease);
        assert!(second.lease(&serial).is_ok());
        store.restrict("A", Reason::Thermal).unwrap();
        fs::write(&store.path, b"malformed").unwrap();
        let fresh = Store::open(&f.config()).unwrap();
        assert!(fresh.board("A").is_none());
        assert!(fresh.policy(&board("A")).cautious);
        let archived = fs::read_dir(store.path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("board-safety-corrupt-")
            })
            .unwrap();
        assert_eq!(fs::read(archived.path()).unwrap(), b"malformed");
    }
    #[test]
    fn linked_or_insecure_state_is_rejected() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.closed("A").unwrap();
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::open(&f.config()).is_err());
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600)).unwrap();
        let copy = f.0.join("copy");
        fs::hard_link(&store.path, &copy).unwrap();
        assert!(Store::open(&f.config()).is_err());
        fs::remove_file(copy).unwrap();
        fs::remove_file(&store.path).unwrap();
        symlink(f.0.join("missing"), &store.path).unwrap();
        assert!(Store::open(&f.config()).is_err());
    }
    #[test]
    fn queued_old_point_cannot_modify_a_successor() {
        let f = Fixture::new();
        let store = std::sync::Arc::new(Store::open(&f.config()).unwrap());
        let old = store.begin_owner("A", point(1450.)).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let queued = store.clone();
        let job = std::thread::spawn(move || {
            wait.recv().unwrap();
            queued.set_point("A", old, point(1500.))
        });
        store.closed("A").unwrap();
        let successor = store.begin_owner("A", point(1450.)).unwrap();
        release.send(()).unwrap();
        assert!(!job.join().unwrap().unwrap());
        assert_eq!(store.owner_token("A"), Some(successor));
        assert_eq!(store.board("A").unwrap().last_point, Some(point(1450.)));
    }
    #[test]
    fn queued_writer_cannot_replace_the_critical_holders_watchdog() {
        let f = Fixture::new();
        let progress = std::sync::Arc::new(timelord_supervisor::progress::Progress::default());
        let store = std::sync::Arc::new(
            Store::open(&f.config())
                .unwrap()
                .monitored(progress.clone()),
        );
        let token = store.begin_owner("A", point(1450.)).unwrap();
        let held = store.state.lock().unwrap();
        let first = store.clone();
        let first = std::thread::spawn(move || first.set_point("A", token, point(1450.)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let id = loop {
            if let Some(work) = progress.snapshot(0, 1).and_then(|snapshot| {
                snapshot.slot(timelord_supervisor::progress::Resource::Persistence)
            }) {
                break work.id;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        };
        let second = store.clone();
        let (started, waiting) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            started.send(()).unwrap();
            second.set_point("A", token, point(1450.))
        });
        waiting.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(
            progress
                .snapshot(0, 2)
                .unwrap()
                .slot(timelord_supervisor::progress::Resource::Persistence)
                .unwrap()
                .id,
            id
        );
        drop(held);
        assert!(first.join().unwrap().unwrap());
        assert!(second.join().unwrap().unwrap());
    }
}

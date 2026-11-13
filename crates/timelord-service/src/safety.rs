//! Durable board admission. An unclean owner never becomes a clean restart merely
//! because its process disappeared. Invalid recovery metadata is archived; fresh
//! initialization, exclusive ownership and proof verification establish current state.
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

const MAX_BYTES: u64 = 65536;
const MAX_BOARDS: usize = 64;
#[cfg(test)]
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Thermal,
    InvalidProof,
    UncleanOwner,
}
impl Reason {
    /// Restrictions only strengthen until verified admission clears them.
    pub fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Thermal, _) | (_, Self::Thermal) => Self::Thermal,
            (Self::InvalidProof, _) | (_, Self::InvalidProof) => Self::InvalidProof,
            _ => Self::UncleanOwner,
        }
    }
}
/// Immutable journal policy shared by restoration, capacity and native startup.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Policy {
    pub restriction: Option<Reason>,
    pub force_safe: bool,
    pub retry_after_unix: u64,
}
impl Policy {
    fn from_saved(saved: &Board, _board: &timelord_core::BoardConfig) -> Self {
        Self {
            restriction: saved.quarantine,
            force_safe: saved.force_safe,
            retry_after_unix: saved.retry_after_unix,
        }
    }
    pub fn hold_reason(self, now: u64) -> Option<String> {
        self.restriction
            .filter(|_| self.retry_after_unix > now)
            .map(|reason| {
                format!(
                    "Retrying after {reason:?}; next attempt in {} seconds",
                    self.retry_after_unix.saturating_sub(now)
                )
            })
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    pub engines: u8,
    pub engine_mask: u8,
    pub host: String,
    pub client_sha256: String,
    pub point: crate::automatic::Point,
}
impl Qualification {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=3).contains(&self.engines)
                && self.engine_mask > 0
                && self.engine_mask <= 7
                && self.engine_mask.count_ones() == u32::from(self.engines),
            "Invalid qualification engine mask"
        );
        ensure!(
            self.host.len() <= 128 && !self.host.is_empty(),
            "Invalid qualification host"
        );
        ensure!(
            self.client_sha256.len() == 64
                && self.client_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid qualification binary hash"
        );
        ensure!(
            self.point.frequency_mhz.is_finite()
                && (200.0..=2200.0).contains(&self.point.frequency_mhz)
                && self.point.voltage_v.is_finite()
                && (0.7..=1.0).contains(&self.point.voltage_v),
            "Invalid qualification operating point"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Board {
    pub owner_active: bool,
    #[serde(default)]
    pub owner_epoch: u64,
    pub quarantine: Option<Reason>,
    pub force_safe: bool,
    pub qualification: Option<Qualification>,
    pub updated_unix: u64,
    pub retry_after_unix: u64,
    pub last_point: Option<crate::automatic::Point>,
    #[serde(default)]
    pub admission_frequency_mhz: Option<f64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    schema_version: u32,
    boards: BTreeMap<String, Board>,
}
impl Default for Saved {
    fn default() -> Self {
        Self {
            schema_version: 1,
            boards: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerToken(u64);

pub struct Store {
    path: PathBuf,
    state: Mutex<Saved>,
    update_order: Mutex<()>,
    healthy: Mutex<BTreeMap<String, OwnerToken>>,
    progress: Option<std::sync::Arc<timelord_supervisor::progress::Progress>>,
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

fn validate_saved(state: &Saved) -> Result<()> {
    ensure!(
        state.schema_version == 1 && state.boards.len() <= MAX_BOARDS,
        "Unsupported or oversized board safety state"
    );
    for (serial, board) in &state.boards {
        ensure!(serial_valid(serial), "Invalid saved board identity");
        if let Some(q) = &board.qualification {
            q.validate()?;
        }
        ensure!(
            board
                .admission_frequency_mhz
                .is_none_or(|f| f.is_finite() && (200.0..=2200.0).contains(&f)),
            "Invalid saved admission frequency"
        );
        if let Some(point) = board.last_point {
            ensure!(
                point.frequency_mhz.is_finite()
                    && (200.0..=2200.0).contains(&point.frequency_mhz)
                    && point.voltage_v.is_finite()
                    && (0.7..=1.0).contains(&point.voltage_v),
                "Invalid saved operating point"
            );
        }
    }
    Ok(())
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
                let parsed = (|| {
                    ensure!(
                        bytes.len() as u64 <= MAX_BYTES,
                        "Board safety state exceeds limit"
                    );
                    let saved: Saved =
                        serde_json::from_slice(&bytes).context("Reading board safety state")?;
                    validate_saved(&saved)?;
                    Ok::<_, anyhow::Error>(saved)
                })();
                match parsed {
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
                        tracing::warn!(%error, path=%archived.display(), "Archived invalid recovery metadata; boards will establish fresh current state");
                        Saved::default()
                    }
                }
            }
        };
        for board in state.boards.values_mut() {
            if board.owner_active {
                // The prior worker did not confirm native-owner shutdown. This
                // also covers a crash during fault-journal persistence.
                board.quarantine.get_or_insert(Reason::UncleanOwner);
                board.force_safe = true;
                board.qualification = None;
                board.owner_active = false;
                if board.quarantine == Some(Reason::UncleanOwner) {
                    board.retry_after_unix = 0;
                }
            }
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
            update_order: Mutex::new(()),
            healthy: Mutex::new(BTreeMap::new()),
            progress: None,
        })
    }
    #[cfg(test)]
    pub fn held_fixture(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        let _guard = self.state.lock().expect("fixture safety");
        let _ = entered.send(());
        let _ = release.recv_timeout(std::time::Duration::from_secs(2));
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
        // Optional learned-profile persistence only supplies evidence. Coalesce
        // its relaxation into required journal work; it never owns this writer.
        let healthy = self
            .healthy
            .lock()
            .expect("healthy evidence")
            .remove(serial);
        if healthy == Some(OwnerToken(board.owner_epoch))
            && board.owner_active
            && board.quarantine.is_none()
        {
            board.force_safe = false;
        }
        change(board);
        board.updated_unix = crate::unix_now();
        if !relaxing {
            // Publish restrictions before I/O, but never publish a coalesced
            // relaxation unless the complete transaction is durable.
            let mut restrictive = next.clone();
            if let Some(old) = self.state.lock().expect("safety").boards.get(serial)
                && old.force_safe
            {
                restrictive
                    .boards
                    .get_mut(serial)
                    .expect("board")
                    .force_safe = true;
            }
            *self.state.lock().expect("safety") = restrictive;
        }
        self.save(&next)?;
        *self.state.lock().expect("safety") = next;
        Ok(true)
    }
    pub(crate) fn policy(&self, board: &timelord_core::BoardConfig) -> Policy {
        board
            .serial
            .as_ref()
            .and_then(|serial| {
                self.state
                    .lock()
                    .expect("safety")
                    .boards
                    .get(serial)
                    .map(|saved| Policy::from_saved(saved, board))
            })
            .unwrap_or_else(|| Policy {
                force_safe: true,
                ..Policy::default()
            })
    }
    pub fn needs_self_test(&self, serial: &str, q: &Qualification) -> bool {
        self.board(serial).is_none_or(|b| {
            b.owner_active || b.quarantine.is_some() || b.qualification.as_ref() != Some(q)
        })
    }
    pub fn begin_owner(&self, serial: &str, point: crate::automatic::Point) -> Result<OwnerToken> {
        ensure!(
            point.frequency_mhz.is_finite()
                && (200.0..=2200.0).contains(&point.frequency_mhz)
                && point.voltage_v.is_finite()
                && (0.7..=1.0).contains(&point.voltage_v),
            "Invalid native operating point"
        );
        let mut token = OwnerToken(0);
        self.update(serial, false, |b| {
            b.owner_active = true;
            b.owner_epoch = b.owner_epoch.checked_add(1).expect("owner epoch exhausted");
            token = OwnerToken(b.owner_epoch);
            b.last_point = Some(point);
        })?;
        Ok(token)
    }
    /// A matching live owner has established fresh reset/readback and cool
    /// telemetry. This clears recovery restrictions without inventing a closed
    /// owner or claiming an offline proof qualification.
    pub fn prepare_recovery_point(
        &self,
        serial: &str,
        token: OwnerToken,
        point: crate::automatic::Point,
    ) -> Result<bool> {
        ensure!(
            point.frequency_mhz.is_finite()
                && (200. ..=2200.).contains(&point.frequency_mhz)
                && point.voltage_v.is_finite()
                && (0.7..=1.).contains(&point.voltage_v),
            "Invalid recovery operating point"
        );
        self.transition(
            serial,
            false,
            |b| b.owner_active && b.owner_epoch == token.0,
            |b| b.last_point = Some(point),
        )
    }
    pub fn recovered(&self, serial: &str, token: OwnerToken) -> Result<bool> {
        self.transition(
            serial,
            true,
            |b| b.owner_active && b.owner_epoch == token.0,
            |b| {
                b.quarantine = None;
                b.retry_after_unix = 0;
                b.qualification = None;
            },
        )
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
        ensure!(
            point.frequency_mhz.is_finite()
                && (200.0..=2200.0).contains(&point.frequency_mhz)
                && point.voltage_v.is_finite()
                && (0.7..=1.0).contains(&point.voltage_v),
            "Invalid native operating point"
        );
        self.transition(
            serial,
            false,
            |b| b.owner_active && b.owner_epoch == token.0 && b.quarantine.is_none(),
            |b| b.last_point = Some(point),
        )
    }
    pub fn closed(&self, serial: &str) -> Result<()> {
        self.update(serial, true, |b| b.owner_active = false)
    }
    pub fn quarantine(&self, serial: &str, reason: Reason) -> Result<()> {
        self.update(serial, false, |b| {
            let first_failure =
                b.quarantine.is_none() || b.quarantine == Some(Reason::UncleanOwner);
            let incoming = reason;
            let reason = b
                .quarantine
                .map_or(reason, |previous| previous.merge(reason));
            if first_failure
                && matches!(incoming, Reason::Thermal | Reason::InvalidProof)
                && let Some(point) = b.last_point
            {
                let next = (point.frequency_mhz - 50.).max(200.);
                b.admission_frequency_mhz =
                    Some(b.admission_frequency_mhz.map_or(next, |old| old.min(next)));
            }
            b.quarantine = Some(reason);
            b.force_safe = true;
            b.qualification = None;
            b.retry_after_unix = crate::unix_now().saturating_add(match reason {
                Reason::Thermal => 30,
                Reason::InvalidProof => 10,
                Reason::UncleanOwner => 0,
            });
        })
    }
    /// Called only after fresh cool telemetry, verified offline proofs on every
    /// selected engine, and confirmed retirement of that test's native owner.
    pub fn qualify(&self, serial: &str, q: Qualification) -> Result<()> {
        q.validate()?;
        self.update(serial, true, |b| {
            b.qualification = Some(q);
            b.quarantine = None;
            b.owner_active = false;
            b.retry_after_unix = 0;
        })
    }
    pub fn healthy_profile_saved(&self, serial: &str, token: OwnerToken) -> Result<()> {
        ensure!(serial_valid(serial), "Invalid persistent board serial");
        let mut pending = self.healthy.lock().expect("healthy evidence");
        ensure!(
            pending.contains_key(serial) || pending.len() < MAX_BOARDS,
            "Healthy evidence registry is full"
        );
        pending.insert(serial.into(), token);
        Ok(())
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
    fn qualification() -> Qualification {
        Qualification {
            engines: 1,
            engine_mask: 2,
            host: "fixture".into(),
            client_sha256: "a".repeat(64),
            point: crate::automatic::Point {
                frequency_mhz: 1450.,
                voltage_v: 1.,
            },
        }
    }
    #[test]
    fn one_policy_projection_covers_manual_holds_cooldown_and_safe_startup() {
        let fixture = Fixture::new();
        let store = Store::open(&fixture.config()).unwrap();
        let mut board: timelord_core::BoardConfig = toml::from_str("id='a'\nname='A'\nserial='A'\nclient_port=18110\nprofile='manual'\nfrequency_mhz=1450.0\nvoltage_v=1.0").unwrap();
        store.begin_owner("A", qualification().point).unwrap();
        store.quarantine("A", Reason::InvalidProof).unwrap();
        let policy = store.policy(&board);
        assert_eq!(policy.restriction, Some(Reason::InvalidProof));
        assert!(policy.force_safe);
        assert!(
            policy.hold_reason(u64::MAX).is_none(),
            "Manual faults must retry without a settings change"
        );
        board.frequency_mhz = 1460.;
        assert!(store.policy(&board).hold_reason(u64::MAX).is_none());
        assert!(store.policy(&board).hold_reason(0).is_some());
        store.qualify("A", qualification()).unwrap();
        let policy = store.policy(&board);
        assert_eq!(policy.restriction, None);
        assert_eq!(policy.retry_after_unix, 0);
        assert!(
            policy.force_safe,
            "verified admission does not silently relax force_safe"
        );
    }
    #[test]
    fn fault_and_unclean_ownership_survive_worker_replacement_and_verified_admission_is_explicit() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.begin_owner("A", qualification().point).unwrap();
        let recovered = Store::open(&f.config()).unwrap();
        let board = recovered.board("A").unwrap();
        assert_eq!(board.quarantine, Some(Reason::UncleanOwner));
        assert_eq!(board.retry_after_unix, 0);
        assert!(board.force_safe);
        assert!(board.qualification.is_none());
        recovered.quarantine("A", Reason::InvalidProof).unwrap();
        drop(recovered);
        let recovered = Store::open(&f.config()).unwrap();
        assert_eq!(
            recovered.board("A").unwrap().quarantine,
            Some(Reason::InvalidProof)
        );
        recovered.qualify("A", qualification()).unwrap();
        recovered.begin_owner("A", qualification().point).unwrap();
        recovered.closed("A").unwrap();
        let clean = Store::open(&f.config()).unwrap();
        assert!(clean.board("A").unwrap().quarantine.is_none());
        assert!(clean.board("A").unwrap().force_safe);
        clean.begin_owner("A", qualification().point).unwrap();
        clean
            .healthy_profile_saved("A", clean.owner_token("A").unwrap())
            .unwrap();
        clean.closed("A").unwrap();
        assert!(
            !Store::open(&f.config())
                .unwrap()
                .board("A")
                .unwrap()
                .force_safe
        );
    }
    #[test]
    fn thermal_quarantine_wins_in_both_orders_and_keeps_its_cooldown() {
        for order in [
            [Reason::Thermal, Reason::InvalidProof],
            [Reason::InvalidProof, Reason::Thermal],
        ] {
            let f = Fixture::new();
            let store = Store::open(&f.config()).unwrap();
            store.begin_owner("A", qualification().point).unwrap();
            for reason in order {
                store.quarantine("A", reason).unwrap();
            }
            let now = crate::unix_now();
            store.quarantine("A", Reason::UncleanOwner).unwrap();
            let board = Store::open(&f.config()).unwrap().board("A").unwrap();
            assert_eq!(board.quarantine, Some(Reason::Thermal));
            assert!(board.retry_after_unix >= now + 30);
            assert!(board.qualification.is_none());
        }
    }
    #[test]
    fn repeated_bad_points_lower_admission_but_transport_faults_do_not() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let mut point = qualification().point;
        store.begin_owner("A", point).unwrap();
        store.quarantine("A", Reason::UncleanOwner).unwrap();
        assert!(store.board("A").unwrap().admission_frequency_mhz.is_none());
        store.quarantine("A", Reason::InvalidProof).unwrap();
        assert_eq!(
            store.board("A").unwrap().admission_frequency_mhz,
            Some(1400.)
        );
        point.frequency_mhz = 1400.;
        let token = store.begin_owner("A", point).unwrap();
        // A fresh initialized owner resumes; a new actual bad proof can lower
        // the point again. Re-checkpointing the old fault cannot do so.
        store.recovered("A", token).unwrap();
        store.quarantine("A", Reason::InvalidProof).unwrap();
        assert_eq!(
            Store::open(&f.config())
                .unwrap()
                .board("A")
                .unwrap()
                .admission_frequency_mhz,
            Some(1350.)
        );
        assert!(
            store
                .begin_owner(
                    "A",
                    crate::automatic::Point {
                        frequency_mhz: f64::NAN,
                        voltage_v: 1.
                    }
                )
                .is_err()
        );
        assert!(
            store
                .begin_owner(
                    "A",
                    crate::automatic::Point {
                        frequency_mhz: 1450.,
                        voltage_v: 1.01
                    }
                )
                .is_err()
        );
        assert!(store.board("B").is_none());
    }
    #[test]
    fn production_lease_home_is_owned_and_outside_temporary_storage() {
        let home = account_home().unwrap();
        assert!(!home.starts_with("/tmp") && !home.starts_with("/private/tmp"));
        assert!(fs::metadata(home).unwrap().is_dir());
    }
    #[test]
    fn leases_are_exclusive_and_invalid_metadata_is_archived_for_fresh_recovery() {
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
        store.quarantine("A", Reason::Thermal).unwrap();
        assert!(store.board("B").is_none());
        fs::write(&store.path, b"malformed").unwrap();
        let fresh = Store::open(&f.config()).unwrap();
        assert!(fresh.board("A").is_none());
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
        assert!(!store.path.exists());
    }
    #[test]
    fn failed_persistence_cannot_clear_quarantine_or_unclean_ownership() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.begin_owner("A", qualification().point).unwrap();
        let old = f.0.join("saved-directory");
        let directory = store.path.parent().unwrap().to_owned();
        fs::rename(&directory, &old).unwrap();
        fs::write(&directory, b"not a directory").unwrap();
        assert!(store.quarantine("A", Reason::InvalidProof).is_err());
        assert!(store.qualify("A", qualification()).is_err());
        assert_eq!(
            store.board("A").unwrap().quarantine,
            Some(Reason::InvalidProof)
        );
        assert!(store.closed("A").is_err());
        assert!(store.board("A").unwrap().owner_active);
        fs::remove_file(&directory).unwrap();
        fs::rename(old, &directory).unwrap();
        let recovered = Store::open(&f.config()).unwrap();
        assert_eq!(
            recovered.board("A").unwrap().quarantine,
            Some(Reason::UncleanOwner)
        );
        assert!(recovered.board("A").unwrap().force_safe);
    }
    #[test]
    fn linked_insecure_and_invalid_qualification_state_cannot_admit_a_board() {
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
        let mut invalid = qualification();
        invalid.engine_mask = 7;
        assert!(store.qualify("A", invalid).is_err());
    }
    #[test]
    fn optional_healthy_evidence_never_waits_for_the_critical_writer() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.begin_owner("A", qualification().point).unwrap();
        store.quarantine("A", Reason::UncleanOwner).unwrap();
        store.qualify("A", qualification()).unwrap();
        let token = store.begin_owner("A", qualification().point).unwrap();
        {
            let _blocked_writer = store.update_order.lock().unwrap();
            store.healthy_profile_saved("A", token).unwrap();
            assert!(store.board("A").unwrap().force_safe);
        }
        store.closed("A").unwrap();
        assert!(!store.board("A").unwrap().force_safe);
    }
    #[test]
    fn delayed_point_cannot_revive_a_retired_owner_or_change_its_successor() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let point = qualification().point;
        let old = store.begin_owner("A", point).unwrap();
        store.closed("A").unwrap();
        let changed = crate::automatic::Point {
            frequency_mhz: 1500.,
            ..point
        };
        assert!(!store.set_point("A", old, changed).unwrap());
        assert!(!store.board("A").unwrap().owner_active);
        let current = store.begin_owner("A", point).unwrap();
        assert!(!store.set_point("A", old, changed).unwrap());
        assert_eq!(store.owner_token("A"), Some(current));
        assert_eq!(store.board("A").unwrap().last_point, Some(point));
        assert!(store.set_point("A", current, changed).unwrap());
        assert_eq!(store.board("A").unwrap().owner_epoch, current.0);
        store.quarantine("A", Reason::Thermal).unwrap();
        assert!(!store.set_point("A", current, point).unwrap());
    }
    #[test]
    fn stale_healthy_evidence_cannot_relax_a_successor() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let point = qualification().point;
        let old = store.begin_owner("A", point).unwrap();
        store.quarantine("A", Reason::InvalidProof).unwrap();
        store.qualify("A", qualification()).unwrap();
        let successor = store.begin_owner("A", point).unwrap();
        store.healthy_profile_saved("A", old).unwrap();
        assert!(store.set_point("A", successor, point).unwrap());
        assert!(store.board("A").unwrap().force_safe);
        store.healthy_profile_saved("A", successor).unwrap();
        assert!(store.set_point("A", successor, point).unwrap());
        assert!(!store.board("A").unwrap().force_safe);
        store.closed("A").unwrap();
        assert!(
            !Store::open(&f.config())
                .unwrap()
                .board("A")
                .unwrap()
                .force_safe
        );
    }
    #[test]
    fn queued_old_point_cannot_modify_a_successor() {
        let f = Fixture::new();
        let store = std::sync::Arc::new(Store::open(&f.config()).unwrap());
        let point = qualification().point;
        let old = store.begin_owner("A", point).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let queued = store.clone();
        let job = std::thread::spawn(move || {
            wait.recv().unwrap();
            queued.set_point(
                "A",
                old,
                crate::automatic::Point {
                    frequency_mhz: 1500.,
                    ..point
                },
            )
        });
        store.closed("A").unwrap();
        let successor = store.begin_owner("A", point).unwrap();
        release.send(()).unwrap();
        assert!(!job.join().unwrap().unwrap());
        assert_eq!(store.owner_token("A"), Some(successor));
        assert_eq!(store.board("A").unwrap().last_point, Some(point));
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
        let token = store.begin_owner("A", qualification().point).unwrap();
        let held = store.state.lock().unwrap();
        let first = store.clone();
        let first = std::thread::spawn(move || first.set_point("A", token, qualification().point));
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
            second.set_point("A", token, qualification().point)
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

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn every_fault_expires_and_only_the_current_live_owner_can_clear_it() {
        for (index, reason) in [Reason::Thermal, Reason::InvalidProof, Reason::UncleanOwner]
            .into_iter()
            .enumerate()
        {
            let path = std::env::temp_dir()
                .join(format!("tl-recovery-policy-{}-{index}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            let store = Store::open(&path.join("config.toml")).unwrap();
            let point = crate::automatic::Point {
                frequency_mhz: 1450.,
                voltage_v: 1.,
            };
            let old = store.begin_owner("fixture", point).unwrap();
            store.quarantine("fixture", reason).unwrap();
            let board: timelord_core::BoardConfig = toml::from_str(
                "id='a'\nname='A'\nserial='fixture'\nclient_port=18110\nprofile='manual'",
            )
            .unwrap();
            assert!(
                store.policy(&board).hold_reason(u64::MAX).is_none(),
                "{reason:?} cannot require intervention"
            );
            let current = store.begin_owner("fixture", point).unwrap();
            assert!(!store.recovered("fixture", old).unwrap());
            assert_eq!(store.board("fixture").unwrap().quarantine, Some(reason));
            assert!(store.recovered("fixture", current).unwrap());
            let record = store.board("fixture").unwrap();
            assert!(record.owner_active);
            assert!(record.quarantine.is_none());
            assert_eq!(record.retry_after_unix, 0);
            assert!(
                record.qualification.is_none(),
                "Initialization must not invent proof evidence"
            );
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

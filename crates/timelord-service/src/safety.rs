//! Durable board admission. An unclean owner never becomes a clean restart merely
//! because its process disappeared. Unknown/corrupt state is not replaced by defaults.
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
    /// Last write-ahead request; this is not evidence of applied or verified hardware work.
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
pub struct Store {
    path: PathBuf,
    state: Mutex<Saved>,
    update_order: Mutex<()>,
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
                ensure!(
                    bytes.len() as u64 <= MAX_BYTES,
                    "Board safety state exceeds limit"
                );
                serde_json::from_slice::<Saved>(&bytes).context("Reading board safety state")?
            }
        };
        ensure!(
            state.schema_version == 1 && state.boards.len() <= MAX_BOARDS,
            "Unsupported or oversized board safety state"
        );
        for (serial, board) in &mut state.boards {
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
            if board.owner_active {
                // The prior worker did not confirm native-owner shutdown. This
                // also covers a crash during fault-journal persistence.
                board.quarantine.get_or_insert(Reason::UncleanOwner);
                board.force_safe = true;
                board.qualification = None;
                board.owner_active = false;
                board.retry_after_unix = board
                    .retry_after_unix
                    .max(crate::unix_now().saturating_add(30));
            }
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
            update_order: Mutex::new(()),
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
        let _ = release.recv_timeout(std::time::Duration::from_secs(5));
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
        self.update_checkpoint(serial, relaxing, true, change)
    }
    fn update_checkpoint(
        &self,
        serial: &str,
        relaxing: bool,
        critical: bool,
        change: impl FnOnce(&mut Board),
    ) -> Result<()> {
        ensure!(serial_valid(serial), "Invalid persistent board serial");
        let _order = self.update_order.lock().expect("safety writer");
        let mut next = self.state.lock().expect("safety").clone();
        ensure!(
            next.boards.contains_key(serial) || next.boards.len() < MAX_BOARDS,
            "Board safety registry is full"
        );
        let board = next.boards.entry(serial.into()).or_default();
        change(board);
        board.updated_unix = crate::unix_now();
        // Restriction survives a write failure in memory. Admission and clearing
        // an owner/fallback cannot take effect until persistence succeeds.
        if !relaxing {
            *self.state.lock().expect("safety") = next.clone();
        }
        self.save(&next, critical)?;
        *self.state.lock().expect("safety") = next;
        Ok(())
    }
    pub fn needs_self_test(&self, serial: &str, q: &Qualification) -> bool {
        self.board(serial).is_none_or(|b| {
            b.owner_active || b.quarantine.is_some() || b.qualification.as_ref() != Some(q)
        })
    }
    pub fn before_open(&self, serial: &str, point: crate::automatic::Point) -> Result<()> {
        ensure!(
            point.frequency_mhz.is_finite()
                && (200.0..=2200.0).contains(&point.frequency_mhz)
                && point.voltage_v.is_finite()
                && (0.7..=1.0).contains(&point.voltage_v),
            "Invalid native operating point"
        );
        self.update(serial, false, |b| {
            b.owner_active = true;
            b.owner_epoch = b.owner_epoch.saturating_add(1);
            b.last_point = Some(point);
        })
    }
    pub fn closed(&self, serial: &str) -> Result<()> {
        self.update(serial, true, |b| b.owner_active = false)
    }
    pub fn quarantine(&self, serial: &str, reason: Reason) -> Result<()> {
        self.update(serial, false, |b| {
            let reason = match (b.quarantine, reason) {
                (Some(Reason::InvalidProof), _) | (_, Reason::InvalidProof) => Reason::InvalidProof,
                (Some(Reason::Thermal), _) | (_, Reason::Thermal) => Reason::Thermal,
                _ => Reason::UncleanOwner,
            };
            if matches!(reason, Reason::Thermal | Reason::InvalidProof)
                && let Some(point) = b.last_point
            {
                let next = (point.frequency_mhz - 50.).max(200.);
                b.admission_frequency_mhz =
                    Some(b.admission_frequency_mhz.map_or(next, |old| old.min(next)));
            }
            b.quarantine = Some(reason);
            b.force_safe = true;
            b.qualification = None;
            b.retry_after_unix =
                crate::unix_now().saturating_add(if reason == Reason::Thermal { 300 } else { 30 });
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
    pub fn healthy_profile_saved(&self, serial: &str, epoch: u64) -> Result<()> {
        // The caller retains the optional checkpoint lane for this actual job.
        // Do not arm or replace the independent critical journal watchdog.
        self.update_checkpoint(serial, true, false, |b| {
            if b.owner_active && b.owner_epoch == epoch && b.quarantine.is_none() {
                b.force_safe = false;
            }
        })
    }
    fn save(&self, state: &Saved, critical: bool) -> Result<()> {
        let _stage = self
            .progress
            .as_ref()
            .filter(|_| critical)
            .map(|p| p.begin(9, 0, 0, timelord_supervisor::progress::Stage::Persistence));
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
    fn fault_and_unclean_ownership_survive_worker_replacement_and_verified_admission_is_explicit() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.before_open("A", qualification().point).unwrap();
        let recovered = Store::open(&f.config()).unwrap();
        let board = recovered.board("A").unwrap();
        assert_eq!(board.quarantine, Some(Reason::UncleanOwner));
        assert!(board.retry_after_unix >= crate::unix_now() + 29);
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
        recovered.before_open("A", qualification().point).unwrap();
        recovered.closed("A").unwrap();
        let clean = Store::open(&f.config()).unwrap();
        assert!(clean.board("A").unwrap().quarantine.is_none());
        assert!(clean.board("A").unwrap().force_safe);
        clean.before_open("A", qualification().point).unwrap();
        clean
            .healthy_profile_saved("A", clean.board("A").unwrap().owner_epoch)
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
    fn optional_healthy_checkpoint_does_not_replace_critical_watchdog_work() {
        use timelord_supervisor::progress::{Progress, Stage};
        let f = Fixture::new();
        let progress = std::sync::Arc::new(Progress::default());
        let store = Store::open(&f.config())
            .unwrap()
            .monitored(progress.clone());
        store.before_open("A", qualification().point).unwrap();
        let epoch = store.board("A").unwrap().owner_epoch;
        let _critical = progress.begin(9, 7, 0, Stage::Persistence);
        let before = progress.snapshot(7, 1).unwrap().work[9].unwrap().id;
        store.healthy_profile_saved("A", epoch).unwrap();
        let after = progress.snapshot(7, 2).unwrap().work[9].unwrap();
        assert_eq!(after.id, before);
        assert_eq!(after.stage, Stage::Persistence);
    }

    #[test]
    fn a_late_profile_save_cannot_clear_a_newer_owners_safety_fallback() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let q = qualification();
        store.qualify("A", q.clone()).unwrap();
        store.before_open("A", q.point).unwrap();
        let old = store.board("A").unwrap().owner_epoch;
        store.quarantine("A", Reason::InvalidProof).unwrap();
        store.healthy_profile_saved("A", old).unwrap();
        assert!(store.board("A").unwrap().force_safe);
        store.qualify("A", q.clone()).unwrap();
        store.before_open("A", q.point).unwrap();
        store.healthy_profile_saved("A", old).unwrap();
        assert!(store.board("A").unwrap().force_safe);
        store
            .healthy_profile_saved("A", store.board("A").unwrap().owner_epoch)
            .unwrap();
        assert!(!store.board("A").unwrap().force_safe);
    }
    #[test]
    fn repeated_bad_points_lower_admission_but_transport_faults_do_not() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        let mut point = qualification().point;
        store.before_open("A", point).unwrap();
        store.quarantine("A", Reason::UncleanOwner).unwrap();
        assert!(store.board("A").unwrap().admission_frequency_mhz.is_none());
        store.quarantine("A", Reason::InvalidProof).unwrap();
        assert_eq!(
            store.board("A").unwrap().admission_frequency_mhz,
            Some(1400.)
        );
        point.frequency_mhz = 1400.;
        store.before_open("A", point).unwrap();
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
                .before_open(
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
                .before_open(
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
    fn admission_is_serial_scoped_and_exclusive_and_corruption_is_not_defaulted() {
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
        assert!(Store::open(&f.config()).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), b"malformed");
    }
    #[test]
    fn failed_persistence_cannot_clear_quarantine_or_unclean_ownership() {
        let f = Fixture::new();
        let store = Store::open(&f.config()).unwrap();
        store.before_open("A", qualification().point).unwrap();
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
}

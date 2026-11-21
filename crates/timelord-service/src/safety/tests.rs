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
    let checked = check.record_failure("A", "check").unwrap();
    assert_eq!(checked.1.consecutive_failures, 1);
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
fn forgive_is_refused_after_an_interleaved_retry_and_accepted_on_a_clean_path() {
    let f = Fixture::new();
    let store = Store::open(&f.config()).unwrap();
    let (before, after) = store.record_failure("A", "x").unwrap();
    let now = crate::unix_now();
    store
        .update_health("A", true, |health| health.retry_now(now))
        .unwrap();
    let retried = store.health("A");
    assert!(!store.forgive("A", &after, before.clone()).unwrap());
    assert_eq!(store.health("A"), retried);

    let clean = Fixture::new();
    let store = Store::open(&clean.config()).unwrap();
    let (before, after) = store.record_failure("A", "x").unwrap();
    assert!(store.forgive("A", &after, before.clone()).unwrap());
    assert_eq!(store.health("A"), before);

    let diagnostic_dir = Fixture::new();
    Store::open(&diagnostic_dir.config())
        .unwrap()
        .record_failure("A", "x")
        .unwrap();
    let production = Store::open(&diagnostic_dir.config()).unwrap();
    let post_failure = production.health("A");
    let diagnostic = Store::open(&diagnostic_dir.config()).unwrap().diagnostic();
    assert!(
        !diagnostic
            .forgive("A", &post_failure, Health::default())
            .unwrap()
    );
    assert_eq!(
        Store::open(&diagnostic_dir.config()).unwrap().health("A"),
        post_failure
    );
}
#[test]
fn forgive_reads_health_under_the_writer_order() {
    let f = Fixture::new();
    let store = std::sync::Arc::new(Store::open(&f.config()).unwrap());
    let (before, after) = store.record_failure("A", "x").unwrap();
    let order = store.update_order.lock().unwrap();
    let queued = store.clone();
    let job = std::thread::spawn(move || queued.forgive("A", &after, before));
    std::thread::sleep(std::time::Duration::from_millis(20));
    let now = crate::unix_now();
    store
        .state
        .lock()
        .unwrap()
        .boards
        .get_mut("A")
        .unwrap()
        .health
        .retry_now(now);
    let retried = store.health("A");
    drop(order);
    assert!(!job.join().unwrap().unwrap());
    assert_eq!(store.health("A"), retried);
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
    assert_eq!(store.journal_reset(), None);
    assert!(store.board("A").is_none());
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
    assert_eq!(fresh.journal_reset(), Some(JournalReset::Unreadable));
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
    assert_eq!(Store::open(&f.config()).unwrap().journal_reset(), None);
}
#[test]
fn a_missing_journal_is_created_and_a_later_gap_is_a_reset() {
    let f = Fixture::new();
    timelord_supervisor::Paths::for_config(&f.config()).unwrap();
    let store = Store::open(&f.config()).unwrap();
    assert_eq!(store.journal_reset(), None);
    let assert_private = |path: &Path| {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    };
    assert_private(&store.path);
    assert_eq!(Store::open(&f.config()).unwrap().journal_reset(), None);
    fs::remove_file(&store.path).unwrap();
    let recreated = Store::open(&f.config()).unwrap();
    assert_eq!(recreated.journal_reset(), Some(JournalReset::Missing));
    assert_private(&recreated.path);
}
#[test]
fn opening_fails_when_the_default_journal_cannot_be_written() {
    let f = Fixture::new();
    let paths = timelord_supervisor::Paths::for_config(&f.config()).unwrap();
    let journal = paths.directory.join("board-safety.json");
    let lock = timelord_supervisor::atomic::write_lock(&journal).unwrap();
    let _held = timelord_supervisor::InstanceLock::acquire(&lock).unwrap();
    let error = Store::open(&f.config()).err().expect("open must fail");
    assert!(
        format!("{error:#}").contains("Persisting default board safety journal"),
        "{error:#}"
    );
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

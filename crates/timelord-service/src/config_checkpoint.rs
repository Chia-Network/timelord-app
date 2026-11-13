//! Preserve the configuration that actually submitted verified work. Invalid
//! operator edits remain untouched; a fallback is explicit in status/progress.
use crate::{Shared, observability};
use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use timelord_core::Config;
use tokio::sync::watch;
const LIMIT: u64 = 1024 * 1024;
pub fn bounded_text(path: &Path, private: bool) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_CLOEXEC | libc::O_NONBLOCK | if private { libc::O_NOFOLLOW } else { 0 },
        )
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= LIMIT,
        "Configuration must be a regular file no larger than 1 MiB"
    );
    if private {
        ensure!(
            meta.uid() == unsafe { libc::geteuid() }
                && meta.nlink() == 1
                && meta.mode() & 0o077 == 0,
            "Saved configuration must be private and owned"
        );
    }
    let mut text = String::new();
    file.take(LIMIT + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= LIMIT,
        "Configuration exceeds size limit"
    );
    Ok(text)
}
fn saved_path(path: &Path) -> Result<PathBuf> {
    Ok(timelord_supervisor::Paths::for_config(path)?
        .directory
        .join("last-good.toml"))
}
#[derive(Default, serde::Deserialize)]
struct RequestedNetwork {
    #[serde(default)]
    node: NetworkOnly,
}
#[derive(Default, serde::Deserialize)]
struct NetworkOnly {
    #[serde(default)]
    network: timelord_core::Network,
    #[serde(default)]
    network_definition: Option<timelord_core::NetworkDefinition>,
}
pub fn load_running(path: &Path) -> Result<(Config, bool)> {
    match bounded_text(path, false) {
        Ok(text) => match crate::parse_config(path, &text) {
            Ok(config) => Ok((config, false)),
            Err(requested) => {
                // Parse only network intent, even when another field is invalid.
                // Unparseable or unknown intent must not silently select a chain.
                let intent: RequestedNetwork = toml::from_str(&text).with_context(|| {
                    format!("Cannot establish requested network; fallback refused: {requested}")
                })?;
                load_saved(path, Some(intent.node), requested)
            }
        },
        // Text that is not UTF-8 was read, so it is malformed, not unreadable.
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() != std::io::ErrorKind::InvalidData) =>
        {
            load_saved(path, None, error)
        }
        Err(error) => Err(error),
    }
}
fn load_saved(
    path: &Path,
    network: Option<NetworkOnly>,
    requested: anyhow::Error,
) -> Result<(Config, bool)> {
    let saved = saved_path(path)?;
    let text = bounded_text(&saved, true).context(
        "Requested configuration invalid and no readable last-good checkpoint is available",
    )?;
    // Checkpoint storage does not change the original relative-path context.
    let config = crate::parse_config(path, &text).with_context(|| {
        format!("Requested configuration and last-good checkpoint are invalid: {requested}")
    })?;
    if let Some(intent) = network {
        let requested_identity =
            timelord_core::ChainIdentity::new(&intent.network, intent.network_definition.as_ref())
                .map_err(anyhow::Error::msg)?;
        let saved_identity = timelord_core::ChainIdentity::new(
            &config.node.network,
            config.node.network_definition.as_ref(),
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            requested_identity == saved_identity,
            "Requested network differs from last-good checkpoint; fallback refused: {requested}"
        );
    }
    tracing::warn!(
        "Requested configuration unavailable; using saved last-good configuration. Run validate to inspect the requested file."
    );
    Ok((config, true))
}
// Optional disk work must not join Tokio's blocking pool: runtime destruction
// waits indefinitely for a running fsync, even after its JoinHandle is aborted.
// The actual writer retains its process-wide permit until it really finishes.
async fn write_until_shutdown(
    write: impl FnOnce() -> Result<bool> + Send + 'static,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<bool>> {
    if *shutdown.borrow() {
        return Ok(None);
    }
    let rx = crate::optional_io::spawn("config-checkpoint", write)?;
    tokio::select! {
        _ = shutdown.changed() => Ok(None),
        result = rx => result.context("Optional configuration writer stopped")?.map(Some),
    }
}
fn eligible(shared: &Shared) -> bool {
    let sent = {
        let state = shared.runtime.lock().expect("runtime");
        state.proofs_delivered > 0 && state.failed_proofs == 0
    };
    sent && observability::work_ready(shared)
}
fn eligible_for_write(
    shared: &Arc<Shared>,
    current: &watch::Receiver<Arc<Shared>>,
    shutdown: &watch::Receiver<bool>,
) -> bool {
    let stopped = *shutdown.borrow();
    let latest = current.borrow().clone();
    !stopped && Arc::ptr_eq(shared, &latest) && eligible(shared)
}
pub async fn record(
    candidate: Config,
    path: PathBuf,
    current: watch::Receiver<Arc<Shared>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {_=shutdown.changed()=>return Ok(()),_=timer.tick()=>{}}
        let shared = current.borrow().clone();
        if !eligible(&shared) {
            continue;
        }
        let process = shared.process.clone();
        let permit = tokio::select! {p=process.progress.arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint).acquire_owned()=>p?,_=shutdown.changed()=>return Ok(())};
        if !eligible_for_write(&shared, &current, &shutdown) {
            continue;
        }
        let candidate = candidate.clone();
        let path = path.clone();
        let writer_current = current.clone();
        let writer_shutdown = shutdown.clone();
        let writer_process = process.clone();
        let writing = shared.clone();
        match write_until_shutdown(
            move || {
                let _permit = permit;
                // Capacity waits and thread scheduling may delay this job.
                // Inspect current evidence at the actual I/O boundary, then release
                // all runtime/watch locks before touching the filesystem.
                if !eligible_for_write(&writing, &writer_current, &writer_shutdown) {
                    return Ok(false);
                }
                let _stage = writer_process.progress.begin(
                    timelord_supervisor::progress::Resource::OptionalCheckpoint,
                    0,
                    0,
                    timelord_supervisor::progress::Stage::OptionalPersistence,
                );
                crate::save(&saved_path(&path)?, &candidate).map(|()| true)
            },
            &mut shutdown,
        )
        .await
        {
            Ok(Some(true)) => {
                if eligible_for_write(&shared, &current, &shutdown) {
                    process.progress.checkpointed();
                }
                return Ok(());
            }
            Ok(Some(false)) => continue,
            Ok(None) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error,"Last-good configuration checkpoint unavailable; proof work continues")
            }
        }
        tokio::select! {_=shutdown.changed()=>return Ok(()),_=tokio::time::sleep(Duration::from_secs(60))=>{}}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_edits_use_only_private_validated_saved_configuration_and_do_not_overwrite_intent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-lkg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.node.host = "saved.example".into();
        crate::save(&path, &config).unwrap();
        let saved = saved_path(&path).unwrap();
        crate::save(&saved, &config).unwrap();
        std::fs::write(&path, "schema_version = 999").unwrap();
        let (loaded, fallback) = load_running(&path).unwrap();
        assert!(fallback);
        assert_eq!(loaded.node.host, "saved.example");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version = 999"
        );
        std::fs::set_permissions(&saved, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_running(&path).is_err());
        std::fs::set_permissions(&saved, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&saved, "also broken").unwrap();
        assert!(load_running(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn fallback_requires_known_matching_network_unless_requested_file_is_unreadable() {
        let dir = std::env::temp_dir().join(format!("tl-lkg-network-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        crate::save(&saved_path(&path).unwrap(), &Config::default()).unwrap();
        for text in [
            "schema_version=999\n[node]\nnetwork='testnet11'",
            "schema_version=999\n[node]\nnetwork='unknown'",
            "invalid TOML",
        ] {
            std::fs::write(&path, text).unwrap();
            assert!(load_running(&path).is_err(), "{text}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        std::fs::write(&path, b"[node]\nnetwork='mainnet'\n\xff").unwrap();
        assert!(load_running(&path).is_err());
        std::fs::write(&path, "schema_version=999\n[node]\nnetwork='mainnet'").unwrap();
        assert!(load_running(&path).unwrap().1);
        std::fs::remove_file(&path).unwrap();
        assert!(load_running(&path).unwrap().1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fallback_never_crosses_custom_chain_definitions_with_the_same_name() {
        let dir = std::env::temp_dir().join(format!("tl-lkg-custom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut config = Config {
            node: crate::network::tests::node(),
            ..Default::default()
        };
        crate::save(&saved_path(&path).unwrap(), &config).unwrap();
        config.schema_version = 999;
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        assert!(load_running(&path).unwrap().1);
        let definition = config.node.network_definition.as_mut().unwrap();
        definition.exported_at = "re-exported later".into();
        definition.chia_version = "another version".into();
        definition.config_overrides.insert(
            "address_prefix".into(),
            "different-prefix".to_owned().into(),
        );
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        assert!(load_running(&path).unwrap().1);
        config
            .node
            .network_definition
            .as_mut()
            .unwrap()
            .constants
            .insert("GENESIS_CHALLENGE".into(), "cd".repeat(32).into());
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        assert!(load_running(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stalled_optional_disk_work_does_not_hold_runtime_shutdown_or_release_capacity_early() {
        let permit = Arc::new(tokio::sync::Semaphore::new(1));
        let held = permit.clone().try_acquire_owned().unwrap();
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let (stop, mut shutdown) = watch::channel(false);
        let runtime_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                assert_eq!(
                    write_until_shutdown(
                        move || {
                            let _permit = held;
                            entered.send(()).unwrap();
                            let _ = released.recv();
                            Ok(true)
                        },
                        &mut shutdown
                    )
                    .await
                    .unwrap(),
                    None
                );
            });
            drop(runtime);
            finished.send(()).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        stop.send(true).unwrap();
        let stopped = done.recv_timeout(Duration::from_secs(2));
        assert_eq!(permit.available_permits(), 0);
        release.send(()).unwrap();
        stopped.expect("runtime drop waited for optional disk work");
        runtime_thread.join().unwrap();
    }
    #[test]
    fn relative_config_checkpoint_keeps_original_profile_and_credential_paths() {
        let root = PathBuf::from(format!(".tl-lkg-paths-{}", std::process::id()));
        for directory in [root.clone(), root.join("nested")] {
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("config.toml");
            crate::save(&path, &Config::default()).unwrap();
            let mut candidate = crate::load(&path).unwrap();
            crate::set_default_automatic_state(&mut candidate, &path).unwrap();
            let parent = directory.canonicalize().unwrap();
            let expected = parent.join("automatic-profiles.json");
            assert_eq!(candidate.hardware.automatic_state.as_ref(), Some(&expected));
            let saved = saved_path(&path).unwrap();
            crate::save(&saved, &candidate).unwrap();
            std::fs::write(&path, "schema_version = 999").unwrap();
            let (fallback, used_saved) = load_running(&path).unwrap();
            assert!(used_saved);
            assert_eq!(fallback.hardware.automatic_state, Some(expected));

            // Relative checkpoint fields also belong to the original config,
            // including when the operator has removed that file altogether.
            candidate.hardware.automatic_state = Some("learned/profiles.json".into());
            candidate.node.certificate = Some("credentials/node.crt".into());
            candidate.node.private_key = Some("credentials/node.key".into());
            candidate.node.ca_certificate = Some("credentials/ca.crt".into());
            crate::save(&saved, &candidate).unwrap();
            std::fs::remove_file(&path).unwrap();
            let (fallback, used_saved) = load_running(&path).unwrap();
            assert!(used_saved);
            assert_eq!(
                fallback.hardware.automatic_state,
                Some(parent.join("learned/profiles.json"))
            );
            assert_eq!(
                fallback.node.certificate,
                Some(parent.join("credentials/node.crt"))
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn checkpoint_preserves_proven_work_despite_observer_failure_but_rejects_production_failure()
     {
        let dir = std::env::temp_dir().join(format!("tl-lkg-work-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.node.certificate = Some(dir.join("fixture.crt"));
        config.node.private_key = Some(dir.join("fixture.key"));
        config.node.ca_certificate = Some(dir.join("fixture-ca.crt"));
        config.hardware.boards =
            vec![toml::from_str("id='a'\nname='A'\nclient_port=18110").unwrap()];
        let shared = Arc::new(Shared {
            native: None,
            assignments: config.assignments().unwrap(),
            config: config.clone(),
            clients: Default::default(),
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let process = shared.process.clone();
        process.fail(); // A failed scrape listener leaves proof production running.
        let (current_tx, current) = watch::channel(shared.clone());
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(record(config.clone(), path.clone(), current, rx));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!saved_path(&path).unwrap().exists());
        {
            let mut state = shared.runtime.lock().unwrap();
            state.node = "authenticated".into();
            state.scheduler_initialized = true;
            state.proofs_delivered = 1;
            state.boards.insert(
                "a".into(),
                crate::runtime::HardwareState {
                    lifecycle: {
                        let mut owner = crate::board_lifecycle::BoardLifecycle::fixture(
                            crate::board_lifecycle::Phase::Connected,
                            true,
                            None,
                            None,
                            1,
                        );
                        owner.verified(1, 0);
                        owner.verified(1, 1);
                        owner
                    },
                    ..Default::default()
                },
            );
        }
        shared
            .clients
            .lock()
            .unwrap()
            .extend([("a".into(), 0), ("a".into(), 1)]);
        assert!(!observability::ready(&shared));
        assert!(eligible(&shared));
        tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            process
                .progress
                .snapshot(1, 1)
                .unwrap()
                .configuration_checkpointed
        );
        std::fs::write(&path, "schema_version = 999").unwrap();
        assert!(load_running(&path).unwrap().1);
        assert!(
            !observability::ready(&shared),
            "promotion cannot clear observer failure"
        );
        for essential in [false, true] {
            let waiting_process = crate::operational::State::fixture();
            waiting_process.fail(); // Observer failure alone must remain acceptable.
            let mut waiting_state = crate::runtime::RuntimeState::new(false);
            {
                let proven = shared.runtime.lock().unwrap();
                waiting_state.node = proven.node.clone();
                waiting_state.scheduler_initialized = proven.scheduler_initialized;
                waiting_state.proofs_delivered = proven.proofs_delivered;
                waiting_state.boards = proven.boards.clone();
            }
            let waiting = Arc::new(Shared {
                native: None,
                config: shared.config.clone(),
                assignments: shared.assignments.clone(),
                clients: shared.clients.clone(),
                process: waiting_process.clone(),
                scope: crate::runtime::RuntimeScope::Production,
                runtime: Arc::new(std::sync::Mutex::new(waiting_state)),
                started_unix: shared.started_unix,
                service_started_unix: shared.service_started_unix,
            });
            current_tx.send_replace(waiting.clone());
            assert!(eligible(&waiting));
            let capacity = waiting_process
                .progress
                .arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint);
            let held = capacity.acquire_owned().await.unwrap();
            let rejected = dir.join(format!("failed-during-permit-wait-{essential}.toml"));
            let (reject_stop, rx) = watch::channel(false);
            let recording = record(config.clone(), rejected.clone(), current_tx.subscribe(), rx);
            tokio::pin!(recording);
            // First tick passes eligibility and blocks on the held capacity.
            tokio::select! {
                biased;
                result = &mut recording => panic!("record exited before capacity became available: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
            if essential {
                waiting_process.essential_failure();
            } else {
                waiting.runtime.lock().unwrap().failed_proofs = 1;
            }
            drop(held);
            tokio::select! {
                result = &mut recording => panic!("ineligible checkpoint was promoted: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
            reject_stop.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(3), &mut recording)
                .await
                .unwrap()
                .unwrap();
            assert!(!saved_path(&rejected).unwrap().exists());
            assert!(
                !waiting_process
                    .progress
                    .snapshot(1, 1)
                    .unwrap()
                    .configuration_checkpointed
            );
            current_tx.send_replace(shared.clone());
        }
        shared.runtime.lock().unwrap().failed_proofs = 1;
        assert!(!eligible(&shared), "sent work cannot hide a failed proof");
        let rejected = dir.join("proof-failed.toml");
        let (reject_stop, rx) = watch::channel(false);
        let rejecting = tokio::spawn(record(
            config.clone(),
            rejected.clone(),
            current_tx.subscribe(),
            rx,
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        reject_stop.send(true).unwrap();
        rejecting.await.unwrap().unwrap();
        assert!(!saved_path(&rejected).unwrap().exists());
        // Even with a clean proof counter, an essential task exit must reject
        // promotion before the serve loop changes the process to Stopping.
        shared.runtime.lock().unwrap().failed_proofs = 0;
        process.essential_failure();
        assert_eq!(process.snapshot().phase, crate::operational::Phase::Running);
        assert!(!eligible(&shared));
        process.fail();
        assert!(
            !eligible(&shared),
            "observer failure cannot downgrade essential failure"
        );
        let rejected = dir.join("essential-failed.toml");
        let (reject_stop, rx) = watch::channel(false);
        let rejecting = tokio::spawn(record(config, rejected.clone(), current_tx.subscribe(), rx));
        tokio::time::sleep(Duration::from_millis(30)).await;
        reject_stop.send(true).unwrap();
        rejecting.await.unwrap().unwrap();
        assert!(!saved_path(&rejected).unwrap().exists());
        let _ = stop.send(true);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn oversized_and_non_regular_configuration_is_rejected_without_unbounded_reads() {
        let dir = std::env::temp_dir().join(format!("tl-config-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(bounded_text(&dir, false).is_err());
        let path = dir.join("large");
        std::fs::write(&path, vec![b'x'; LIMIT as usize + 1]).unwrap();
        assert!(bounded_text(&path, false).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

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
pub fn load_running(path: &Path) -> Result<(Config, bool)> {
    match crate::load(path) {
        Ok(config) => Ok((config, false)),
        Err(requested) => {
            let saved = saved_path(path)?;
            let text = bounded_text(&saved, true).context(
                "Requested configuration invalid and no readable last-good checkpoint is available",
            )?;
            // The checkpoint retains the original configuration's path context.
            // Moving its bytes into the private state directory must not relocate
            // relative credentials, native libraries or learned profiles.
            let config = crate::parse_config(path, &text).with_context(|| {
                format!("Requested configuration and last-good checkpoint are invalid: {requested}")
            })?;
            tracing::warn!(
                "Requested configuration unavailable; using saved last-good configuration. Run validate to inspect the requested file."
            );
            Ok((config, true))
        }
    }
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
        let sent = shared.runtime.lock().expect("runtime").proofs_delivered > 0;
        if !sent || !observability::ready(&shared) {
            continue;
        }
        let process = shared.runtime.lock().expect("runtime").process.clone();
        let permit = tokio::select! {p=process.progress.arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint).acquire_owned()=>p?,_=shutdown.changed()=>return Ok(())};
        let stage = process.progress.begin(
            timelord_supervisor::progress::Resource::OptionalCheckpoint,
            0,
            0,
            timelord_supervisor::progress::Stage::OptionalPersistence,
        );
        let candidate = candidate.clone();
        let path = path.clone();
        match tokio::task::spawn_blocking(move || {
            let (_permit, _stage) = (permit, stage);
            crate::save(&saved_path(&path)?, &candidate)
        })
        .await?
        {
            Ok(()) => {
                process.progress.checkpointed();
                return Ok(());
            }
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
        std::fs::write(&path, "invalid requested edit").unwrap();
        let (loaded, fallback) = load_running(&path).unwrap();
        assert!(fallback);
        assert_eq!(loaded.node.host, "saved.example");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "invalid requested edit"
        );
        std::fs::set_permissions(&saved, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_running(&path).is_err());
        std::fs::set_permissions(&saved, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&saved, "also broken").unwrap();
        assert!(load_running(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
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
            std::fs::write(&path, "invalid requested edit").unwrap();
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
    async fn checkpoint_requires_actual_live_sent_work_and_recovers_an_invalid_edit() {
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
            assignments: config.assignments().unwrap(),
            config: config.clone(),
            clients: Default::default(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let process = shared.runtime.lock().unwrap().process.clone();
        let (_current, current) = watch::channel(shared.clone());
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
        std::fs::write(&path, "broken edited config").unwrap();
        assert!(load_running(&path).unwrap().1);
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

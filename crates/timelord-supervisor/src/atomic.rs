//! One bounded staging file per target, protected by a stable writer lock. A
//! crash leaves a reusable pending file, never a new PID-named orphan per restart.
use crate::InstanceLock;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
fn sidecar(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = OsString::from(".");
    name.push(path.file_name().context("Atomic target needs a filename")?);
    name.push(suffix);
    Ok(path.with_file_name(name))
}
pub fn pending(path: &Path) -> Result<PathBuf> {
    sidecar(path, ".pending")
}
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= 64 * 1024 * 1024,
        "Atomic state exceeds 64 MiB limit"
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let _lock = InstanceLock::acquire(&sidecar(path, ".write.lock")?)?;
    let temporary = pending(path)?;
    match fs::symlink_metadata(&temporary) {
        Ok(m) => {
            ensure!(
                m.is_file()
                    && m.uid() == unsafe { libc::geteuid() }
                    && m.nlink() == 1
                    && m.mode() & 0o077 == 0,
                "Pending state must be a private owned regular file"
            );
            fs::remove_file(&temporary)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&temporary);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crashed_staging_is_reused_and_bad_staging_cannot_replace_the_saved_value() {
        let dir = std::env::temp_dir().join(format!("tl-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        write(&path, b"old").unwrap();
        let pending = pending(&path).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
            .unwrap();
        file.write_all(b"interrupted").unwrap();
        drop(file);
        write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(!pending.exists());
        fs::create_dir(&pending).unwrap();
        assert!(write(&path, b"bad").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_dir_all(dir).unwrap();
    }
}

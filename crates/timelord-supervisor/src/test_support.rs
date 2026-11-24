//! Fixtures and process probes shared by the unit tests.
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};

/// Write an executable fixture and return its path. The write runs in a child
/// process because Linux refuses to exec a file while any process holds it open
/// for writing: a descriptor opened here can be inherited by an unrelated
/// test's `fork` until that child execs, which makes the fixture's own exec
/// fail with ETXTBSY. The child owns the only writable descriptor, and waiting
/// for it closes that descriptor before the fixture is ever exec'd.
pub(crate) fn script_helper(dir: &Path, name: &str, body: &str) -> PathBuf {
    let helper = dir.join(name);
    let mut writer = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "fixture"])
        .arg(&helper)
        .stdin(Stdio::piped())
        .spawn()
        .expect("fixture writer");
    writer
        .stdin
        .take()
        .expect("fixture stdin")
        .write_all(body.as_bytes())
        .expect("fixture body");
    assert!(writer.wait().expect("fixture writer").success());
    helper
}

/// Whether `pid` was reaped: it is no longer a child this process can wait for.
/// Unlike `kill(pid, 0)`, this cannot touch a process that reused the PID.
pub(crate) fn reaped(pid: i32) -> bool {
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    waited == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn script_helper_writes_the_whole_executable() {
        let dir = std::env::temp_dir().join(format!("tl-script-helper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let helper = script_helper(&dir, "worker", "#!/bin/sh\nexit 3\n");
        assert_eq!(fs::read_to_string(&helper).unwrap(), "#!/bin/sh\nexit 3\n");
        assert_eq!(
            std::os::unix::fs::MetadataExt::mode(&fs::symlink_metadata(&helper).unwrap()) & 0o777,
            0o700
        );
        assert_eq!(
            std::process::Command::new(&helper).status().unwrap().code(),
            Some(3)
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

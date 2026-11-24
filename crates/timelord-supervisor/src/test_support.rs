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

/// Whether `pid` is no longer a child this process can wait for. Unlike
/// `kill(pid, 0)` this never signals a live process, but a PID reused by
/// another test's child is still this process's child, so in that rare case
/// the probe would reap that child instead.
pub(crate) fn reaped(pid: i32) -> bool {
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    waited == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

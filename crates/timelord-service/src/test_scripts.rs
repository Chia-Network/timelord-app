//! Test-only helper for materializing shell fixtures.
//!
//! The write runs in a child because Linux refuses to exec a file while any
//! process holds it open for writing: a descriptor opened here can be inherited
//! by an unrelated test's `fork` until that child execs, which makes the
//! fixture's own exec fail with ETXTBSY. The child owns the only writable
//! descriptor, and waiting for it closes that descriptor before the fixture is
//! ever exec'd.
// See `timelord-supervisor/src/test_support.rs` (`script_helper`).
pub(crate) fn write_executable_script(path: &std::path::Path, contents: &str) {
    use std::{io::Write, process::Stdio};
    let mut writer = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "fixture"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("fixture writer");
    writer
        .stdin
        .take()
        .expect("fixture stdin")
        .write_all(contents.as_bytes())
        .expect("fixture body");
    assert!(
        writer.wait().expect("fixture writer").success(),
        "{}",
        path.display(),
    );
}

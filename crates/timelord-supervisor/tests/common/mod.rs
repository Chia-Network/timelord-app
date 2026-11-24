// The unit tests use the same fixture writer; one copy, two crates.
#[allow(dead_code)]
#[path = "../../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use timelord_supervisor::{Control, Status, request};
use tokio::{
    process::{Child, Command},
    time::sleep,
};

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) struct Fixture(pub(crate) PathBuf);
impl Fixture {
    pub(crate) fn new() -> Self {
        let path = Path::new("/tmp").join(format!(
            "tl-sup-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub(crate) fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }
    pub(crate) fn helper(&self, body: &str) -> PathBuf {
        self.script(
            "worker",
            &format!(
                "#!/bin/sh\nif [ \"$1\" = validate ]; then [ ! -e \"$0.reject\" ]; exit $?; fi\necho $$ >> '{}'\n{body}\n",
                self.0.join("pids").display()
            ),
        )
    }
    /// Write an executable fixture script through the shared child-process
    /// writer, so a concurrent test's `fork` cannot inherit a writable
    /// descriptor and make the exec fail with ETXTBSY.
    pub(crate) fn script(&self, name: &str, body: &str) -> PathBuf {
        test_support::script_helper(&self.0, name, body)
    }
    pub(crate) fn supervisor(&self, helper: &Path) -> Child {
        self.supervisor_with_monitor(helper, None)
    }
    pub(crate) fn supervisor_with_monitor(&self, helper: &Path, monitor: Option<u16>) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_timelord-supervisor"));
        if let Some(port) = monitor {
            command.args([
                "run",
                "--monitor-listen",
                &format!("127.0.0.1:{port}"),
                "--network",
                "testnet11",
            ]);
        } else {
            command.arg("run");
        }
        command
            .arg("--config")
            .arg(self.config())
            .arg("--worker")
            .arg(helper)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }
    pub(crate) async fn status(&self, predicate: impl Fn(&Status) -> bool) -> Status {
        // Pause and Reload return before retirement completes; allow for a
        // loaded CI host while a worker family retires in the background.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(status) = request(&self.config(), Control::Status, None).await
                    && predicate(&status)
                {
                    break status;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("supervisor did not reach requested fixture state")
    }
    pub(crate) async fn terminate(&self, mut child: Child) {
        unsafe {
            libc::kill(child.id().unwrap() as i32, libc::SIGTERM);
        }
        let exit = tokio::time::timeout(Duration::from_secs(4), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(exit.success());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(pids) = fs::read_to_string(self.0.join("pids")) {
            for pid in pids
                .lines()
                .filter_map(|p| p.parse::<i32>().ok())
                .filter(|p| *p > 1)
            {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                };
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

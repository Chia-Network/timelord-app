//! Own a reserved process-group identity through confirmed retirement.
use anyhow::{Context, Result};
use std::{process::ExitStatus, time::Duration};
use tokio::{
    process::{Child, Command},
    time::Instant,
};

pub(super) struct OwnedProcess {
    // Presence owns the right to signal the group. Successful reaping consumes
    // that right; callers cannot hand-disarm Drop or fabricate a live owner.
    child: Option<Child>,
    pid: i32,
    #[cfg(test)]
    pub(super) wait_errors: std::sync::Mutex<std::collections::VecDeque<i32>>,
}
impl OwnedProcess {
    pub(super) fn spawn(command: &mut Command) -> Result<Self> {
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        let pid = child.id().context("Owned process has no PID")? as i32;
        Ok(Self {
            child: Some(child),
            pid,
            #[cfg(test)]
            wait_errors: Default::default(),
        })
    }
    pub(super) fn pid(&self) -> u32 {
        self.pid as u32
    }
    pub(super) fn signal_group(&self, signal: i32) {
        if self.child.is_some() {
            unsafe {
                libc::kill(-self.pid, signal);
            }
        }
    }
    pub(super) fn exited(&self) -> Result<bool> {
        if self.child.is_none() {
            return Ok(true);
        }
        // WNOWAIT reserves the leader PID until all descendants are terminated,
        // so a reused group can never receive a late cleanup signal.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if let Err(error) = self.waitid(&mut info) {
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error).context("Observing owned process exit without reaping");
        }
        #[cfg(target_os = "macos")]
        let pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let pid = unsafe { info.si_pid() };
        // Darwin may report a stopped owner despite WEXITED.
        Ok(pid != 0
            && matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ))
    }
    fn waitid(&self, info: &mut libc::siginfo_t) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(error) = self.wait_errors.lock().expect("wait fixture").pop_front() {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    pub(super) async fn wait_for_exit(&self, limit: Duration) -> Result<bool> {
        let deadline = Instant::now() + limit;
        while !self.exited()? && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.exited()
    }
    pub(super) async fn kill_and_reap(&mut self, limit: Duration) -> Result<ExitStatus> {
        // Never reap before terminating descendants: the leader reserves the
        // group's identity, even if it has already exited successfully.
        self.signal_group(libc::SIGKILL);
        let child = self
            .child
            .as_mut()
            .context("Owned process already retired")?;
        let status = tokio::time::timeout(limit, child.wait())
            .await
            .context("Owned process did not retire after SIGKILL")??;
        self.child.take();
        Ok(status)
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        self.signal_group(libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn exited_leader_stays_reserved_until_family_retirement() {
        let mut owner =
            OwnedProcess::spawn(Command::new("/bin/sh").arg("-c").arg("exit 0")).unwrap();
        let pid = owner.pid() as i32;
        assert!(owner.wait_for_exit(Duration::from_secs(2)).await.unwrap());
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "Leader remains an unreaped reservation"
        );
        assert!(
            owner
                .kill_and_reap(Duration::from_secs(2))
                .await
                .unwrap()
                .success()
        );
        assert!(owner.child.is_none());
        assert!(owner.exited().unwrap());
    }
    #[tokio::test]
    async fn observer_timeout_keeps_owner_armed_and_reaping_is_single_use() {
        let mut owner =
            OwnedProcess::spawn(Command::new("/bin/sh").arg("-c").arg("exec sleep 60")).unwrap();
        assert!(!owner.wait_for_exit(Duration::ZERO).await.unwrap());
        assert!(owner.child.is_some());
        owner.kill_and_reap(Duration::from_secs(2)).await.unwrap();
        assert!(owner.kill_and_reap(Duration::from_secs(2)).await.is_err());
    }
}

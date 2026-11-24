//! Retirement evidence outlives soft deadlines and stream cancellation.
use anyhow::Result;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
};

#[derive(Debug)]
pub(super) struct Retirement {
    pub reaped: bool,
    pub acknowledged: bool,
    pub forced: bool,
}
impl Retirement {
    pub fn confirmed(&self) -> bool {
        if self.forced {
            tracing::warn!(
                "Native process required forced cleanup; evaluating recorded hardware evidence"
            );
        }
        self.reaped && self.acknowledged
    }
}

struct Readers(tokio::task::AbortHandle, tokio::task::AbortHandle);
impl Drop for Readers {
    fn drop(&mut self) {
        self.0.abort();
        self.1.abort();
    }
}

pub(super) async fn retire(
    child: &mut Child,
    stdout: impl AsyncRead + Unpin + Send + 'static,
    stderr: impl AsyncRead + Unpin + Send + 'static,
    sequence: u64,
    initial_evidence: bool,
) -> Retirement {
    let evidence = Arc::new(AtomicBool::new(initial_evidence));
    let confirmed = evidence.clone();
    let mut output = tokio::spawn(async move {
        let mut records = crate::bounded_lines::Lines::new(tokio::io::BufReader::new(stdout));
        while let Ok(Some(record)) = records.next_line().await {
            if let crate::bounded_lines::Line::Text(line) = record {
                let expected = format!("TLSTOPACK 1 {sequence} 1 1");
                if records.terminated() && (line == expected || line == "TLSTOPACK 1 0 1 1") {
                    confirmed.store(true, Ordering::Release);
                }
            }
        }
    });
    let confirmed = evidence.clone();
    let mut diagnostics = tokio::spawn(async move {
        let mut source = stderr;
        // Record evidence as soon as it arrives, independently of EOF/reaping.
        drain_shutdown_observed(&mut source, &confirmed).await
    });
    let _readers = Readers(output.abort_handle(), diagnostics.abort_handle());
    let mut forced = false;
    let mut reaped = matches!(
        tokio::time::timeout(Duration::from_secs(5), child.wait()).await,
        Ok(Ok(_))
    );
    if !reaped {
        tracing::warn!("Native cleanup crossed soft deadline; continuing acknowledgement readers");
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        reaped = matches!(
            tokio::time::timeout(Duration::from_secs(10), child.wait()).await,
            Ok(Ok(_))
        );
    }
    if !reaped {
        forced = true;
        let _ = child.start_kill();
        // A process stuck in an uninterruptible USB call may not die promptly.
        // Never wait forever: report it unreaped (the next start is cautious)
        // and let the runtime's orphan reaper collect it later.
        reaped = matches!(
            tokio::time::timeout(Duration::from_secs(10), child.wait()).await,
            Ok(Ok(_))
        );
        if !reaped {
            tracing::error!("Native process did not exit after SIGKILL; continuing without it");
        }
    }
    // A descendant retaining a pipe cannot erase already observed evidence or
    // leave a background drain task alive after the owner has been reaped.
    if tokio::time::timeout(Duration::from_secs(1), &mut output)
        .await
        .is_err()
    {
        output.abort();
    }
    if tokio::time::timeout(Duration::from_secs(1), &mut diagnostics)
        .await
        .is_err()
    {
        diagnostics.abort();
    }
    let retired = Retirement {
        reaped,
        acknowledged: evidence.load(Ordering::Acquire),
        forced,
    };
    tracing::info!(?retired, "Native owner retired");
    retired
}

#[cfg(test)]
pub(super) async fn drain_shutdown(source: &mut (impl AsyncRead + Unpin)) -> Result<bool> {
    drain_shutdown_observed(source, &AtomicBool::new(false)).await
}

#[cfg(test)]
pub(super) async fn drain_shutdown_diagnostics(
    source: &mut (impl AsyncRead + Unpin),
    observed: &AtomicBool,
) -> Result<(bool, Vec<String>)> {
    let mut records = Vec::new();
    let confirmed =
        drain_shutdown_records(source, observed, |record| records.push(record.into())).await?;
    Ok((confirmed, records))
}

// Native log prefixes are permitted; payload fields and LF/CRLF framing must
// match exactly. An unterminated or truncated record is never confirmation.
async fn drain_shutdown_observed(
    source: &mut (impl AsyncRead + Unpin),
    observed: &AtomicBool,
) -> Result<bool> {
    drain_shutdown_records(source, observed, |record| {
        tracing::info!(record, "Native shutdown diagnostic");
    })
    .await
}

async fn drain_shutdown_records(
    source: &mut (impl AsyncRead + Unpin),
    observed: &AtomicBool,
    mut log: impl FnMut(&str),
) -> Result<bool> {
    let mut buffer = [0u8; 1024];
    let mut drain = ShutdownDrain::default();
    loop {
        let count = source.read(&mut buffer).await?;
        if count == 0 {
            return Ok(drain.confirmed);
        }
        for &byte in &buffer[..count] {
            drain.feed(byte, observed, &mut log);
        }
    }
}

const MARKER: &[u8] = b"Hardware shutdown: engines_paused=1 safe_point_restored=1";
const MAX_LINE: usize = 4096;
const MAX_DIAGNOSTICS: usize = 8;
const DIAGNOSTICS: [&str; 3] = ["safe_readback", "Hardware shutdown:", "VDF client stopped:"];

#[derive(Default)]
struct ShutdownDrain {
    line: Vec<u8>,
    discarding: bool,
    diagnostics: usize,
    confirmed: bool,
}

impl ShutdownDrain {
    // Every newline-terminated record is consumed exactly once. An oversized
    // unterminated record is dropped up to its newline so a suffix can never be
    // mistaken for a diagnostic or a marker.
    fn feed(&mut self, byte: u8, observed: &AtomicBool, log: &mut impl FnMut(&str)) {
        if self.discarding {
            if byte == b'\n' {
                self.discarding = false;
            }
            return;
        }
        if byte != b'\n' {
            if self.line.len() < MAX_LINE {
                self.line.push(byte);
            } else {
                self.line.clear();
                self.discarding = true;
            }
            return;
        }
        let content = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
        if content.ends_with(MARKER) {
            self.confirmed = true;
            observed.store(true, Ordering::Release);
        }
        if self.diagnostics < MAX_DIAGNOSTICS {
            let text = String::from_utf8_lossy(content);
            if DIAGNOSTICS.iter().any(|marker| text.contains(marker)) {
                log(text.trim_end());
                self.diagnostics += 1;
            }
        }
        self.line.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::{io::AsyncWriteExt, process::Command};

    #[tokio::test]
    async fn partial_live_record_survives_reader_handoff() {
        let (mut write, read) = tokio::io::duplex(1024);
        let mut lines = crate::bounded_lines::Lines::new(tokio::io::BufReader::new(read));
        write
            .write_all(b"Hardware shutdown: engines_paused=")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), lines.next_line())
                .await
                .is_err()
        );
        write.write_all(b"1 safe_point_restored=1\n").await.unwrap();
        drop(write);
        assert!(drain_shutdown(&mut lines.into_inner()).await.unwrap());
    }

    #[tokio::test]
    async fn deadline_does_not_discard_early_or_term_fallback_acknowledgements() {
        for early in [true, false] {
            let script = format!(
                r#"trap 'printf "%s\n" "Hardware shutdown: engines_paused=1 safe_point_restored=1" >&2; exit 0' TERM
cat >/dev/null
{}
while :; do sleep .01; done
"#,
                if early {
                    "printf '%s\\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2"
                } else {
                    ""
                }
            );
            let mut child = Command::new("/bin/sh")
                .args(["-c", &script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            // EOF is the legacy fail-safe retirement request in this fixture.
            drop(child.stdin.take());
            let retired = retire(&mut child, stdout, stderr, 1, false).await;
            assert!(retired.confirmed(), "valid evidence was lost: {retired:?}");
            assert!(child.try_wait().unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn structured_ack_is_sequence_bound_and_needs_positive_hardware_fields() {
        for (frame, terminated, expected) in [
            ("TLSTOPACK 1 7 1 1", true, true),
            ("TLSTOPACK 1 0 1 1", true, true),
            ("TLSTOPACK 1 6 1 1", true, false),
            ("TLSTOPACK 1 7 1 0", true, false),
            ("TLSTOPACK 1 7 1 1 extra", true, false),
            ("TLSTOPACK 1 7 1 1", false, false),
        ] {
            let format = if terminated { "%s\\n" } else { "%s" };
            let mut child = Command::new("/bin/sh")
                .args(["-c", &format!("printf '{format}' '{frame}'")])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            assert_eq!(
                retire(&mut child, stdout, stderr, 7, false)
                    .await
                    .confirmed(),
                expected
            );
        }
    }
}

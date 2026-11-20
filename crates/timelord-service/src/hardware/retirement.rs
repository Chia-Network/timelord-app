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
        reaped = child.wait().await.is_ok();
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
// Discard bounded raw chunks while retaining the marker plus a possible CR.
// Native log prefixes are permitted; payload fields and LF/CRLF framing must
// match exactly. An unterminated or truncated record is never confirmation.
async fn drain_shutdown_observed(
    source: &mut (impl tokio::io::AsyncRead + Unpin),
    observed: &AtomicBool,
) -> Result<bool> {
    const MARKER: &[u8] = b"Hardware shutdown: engines_paused=1 safe_point_restored=1";
    let mut buffer = [0u8; 1024];
    let retained = MARKER.len() + 1;
    let mut tail = Vec::with_capacity(buffer.len() + retained);
    let mut confirmed = false;
    let mut diagnostic_records = 0;
    loop {
        let count = source.read(&mut buffer).await?;
        if count == 0 {
            return Ok(confirmed);
        }
        tail.extend_from_slice(&buffer[..count]);
        confirmed |= tail
            .windows(MARKER.len())
            .enumerate()
            .any(|(offset, candidate)| {
                candidate == MARKER
                    && matches!(
                        &tail[offset + MARKER.len()..],
                        [b'\n', ..] | [b'\r', b'\n', ..]
                    )
            });
        if confirmed {
            observed.store(true, Ordering::Release);
        }
        // Shutdown diagnostics were previously discarded, hiding whether the
        // client rejected readback or simply failed to exit. Keep bounded error
        // records in the service log without interpreting them as confirmation.
        for record in tail.split_inclusive(|byte| *byte == b'\n') {
            if diagnostic_records >= 8 || !record.ends_with(b"\n") {
                continue;
            }
            let text = String::from_utf8_lossy(record);
            if ["safe_readback", "Hardware shutdown:", "VDF client stopped:"]
                .iter()
                .any(|marker| text.contains(marker))
            {
                tracing::info!(record = text.trim_end(), "Native shutdown diagnostic");
                diagnostic_records += 1;
            }
        }
        let discard = tail.len().saturating_sub(retained);
        tail.drain(..discard);
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

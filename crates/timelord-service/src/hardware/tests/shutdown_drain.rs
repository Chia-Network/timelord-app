use crate::hardware::retirement::drain_shutdown_diagnostics;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWriteExt};

const MARKER: &str = "Hardware shutdown: engines_paused=1 safe_point_restored=1";

struct Chunked<'a> {
    bytes: &'a [u8],
    chunk: usize,
}

impl AsyncRead for Chunked<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        output: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let count = self.bytes.len().min(self.chunk).min(output.remaining());
        output.put_slice(&self.bytes[..count]);
        self.bytes = &self.bytes[count..];
        std::task::Poll::Ready(Ok(()))
    }
}

async fn diagnostics(bytes: &[u8], chunk: usize) -> (bool, Vec<String>, bool) {
    let mut source = Chunked { bytes, chunk };
    let observed = AtomicBool::new(false);
    let (confirmed, records) = drain_shutdown_diagnostics(&mut source, &observed)
        .await
        .unwrap();
    (confirmed, records, observed.load(Ordering::Acquire))
}

#[tokio::test]
async fn each_record_is_logged_once_across_chunked_reads() {
    let text = format!("{MARKER}\nsafe_readback rejected value=1\nVDF client stopped: id=1\n");
    for chunk in [1, 7, MARKER.len() + 1] {
        let (confirmed, records, observed) = diagnostics(text.as_bytes(), chunk).await;
        assert!(confirmed, "chunk={chunk}");
        assert!(observed, "chunk={chunk}");
        assert_eq!(
            records,
            vec![
                MARKER.to_string(),
                "safe_readback rejected value=1".into(),
                "VDF client stopped: id=1".into(),
            ],
            "chunk={chunk}"
        );
    }
}

#[tokio::test]
async fn framed_marker_confirms_before_eof_across_arbitrary_splits() {
    let marker = MARKER.as_bytes();
    let mut one_byte: Vec<Vec<u8>> = marker.chunks(1).map(<[u8]>::to_vec).collect();
    one_byte.push(b"\n".to_vec());
    let cases: Vec<Vec<Vec<u8>>> = vec![
        one_byte,
        vec![marker.to_vec(), b"\r".to_vec(), b"\n".to_vec()],
        vec![
            b"2026-11-01 INFO ".to_vec(),
            marker[..10].to_vec(),
            marker[10..].to_vec(),
            b"\r\n".to_vec(),
        ],
    ];
    for pieces in cases {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let observed = std::sync::Arc::new(AtomicBool::new(false));
        let drain = tokio::spawn({
            let observed = observed.clone();
            async move { drain_shutdown_diagnostics(&mut reader, &observed).await }
        });
        for piece in &pieces {
            writer.write_all(piece).await.unwrap();
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(Duration::from_millis(500), async {
            while !observed.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("framed marker must confirm before EOF");
        drop(writer);
        let (confirmed, _records) = drain.await.unwrap().unwrap();
        assert!(confirmed);
    }
}

#[tokio::test]
async fn oversized_garbage_cannot_confirm_or_log_a_discarded_suffix() {
    let mut garbage = vec![b'x'; 5000];
    garbage.extend_from_slice(
        b" safe_readback Hardware shutdown: engines_paused=1 safe_point_restored=1 VDF client stopped:",
    );
    garbage.extend_from_slice(&[b'y'; 5000]);

    let result = tokio::time::timeout(Duration::from_secs(2), diagnostics(&garbage, 333))
        .await
        .expect("unterminated oversized line must not grow without bound");
    assert_eq!(result, (false, Vec::new(), false));

    let mut bytes = garbage;
    bytes.push(b'\n');
    bytes.extend_from_slice(MARKER.as_bytes());
    bytes.push(b'\n');
    let (confirmed, records, observed) = diagnostics(&bytes, 1024).await;
    assert!(confirmed);
    assert!(observed);
    assert_eq!(records, vec![MARKER.to_string()]);
}

#[tokio::test]
async fn the_diagnostic_cap_holds_and_never_blocks_marker_confirmation() {
    let mut text = String::new();
    for i in 0..9 {
        text.push_str(&format!("safe_readback {i}\n"));
    }
    let (confirmed, records, observed) = diagnostics(text.as_bytes(), 3).await;
    assert!(!confirmed);
    assert!(!observed);
    assert_eq!(records.len(), 8);
    assert_eq!(records[7], "safe_readback 7");

    let mut text = String::new();
    for i in 0..8 {
        text.push_str(&format!("safe_readback {i}\n"));
    }
    text.push_str(MARKER);
    text.push('\n');
    let (confirmed, records, observed) = diagnostics(text.as_bytes(), 5).await;
    assert!(confirmed);
    assert!(observed);
    assert_eq!(records.len(), 8);
    assert!(!records.iter().any(|record| record == MARKER));
}

use super::chunked::Chunked;
use crate::bounded_lines::Lines;
use crate::hardware::retirement::drain_shutdown_diagnostics;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncWriteExt, BufReader};

const MARKER: &str = "Hardware shutdown: engines_paused=1 safe_point_restored=1";

async fn diagnostics(bytes: &[u8], chunk: usize) -> (bool, Vec<String>) {
    let mut lines = Lines::new(BufReader::with_capacity(
        chunk.max(1),
        Chunked { bytes, chunk },
    ));
    let observed = AtomicBool::new(false);
    drain_shutdown_diagnostics(&mut lines, &observed)
        .await
        .unwrap()
}

#[tokio::test]
async fn each_record_is_logged_once_across_chunked_reads() {
    let text = format!("{MARKER}\nsafe_readback rejected value=1\nVDF client stopped: id=1\n");
    for chunk in [1, 7, MARKER.len() + 1] {
        let (confirmed, records) = diagnostics(text.as_bytes(), chunk).await;
        assert!(confirmed, "chunk={chunk}");
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
        let (mut writer, reader) = tokio::io::duplex(1024);
        let observed = Arc::new(AtomicBool::new(false));
        let drain = tokio::spawn({
            let observed = observed.clone();
            async move {
                let mut lines = Lines::new(BufReader::new(reader));
                drain_shutdown_diagnostics(&mut lines, &observed).await
            }
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
    // Put the diagnostic words in the first bytes of the discarded tail, where
    // a second overflow cannot wipe them before this record is discarded.
    let mut garbage = vec![b'x'; 4096];
    garbage.extend_from_slice(
        b" safe_readback Hardware shutdown: engines_paused=1 safe_point_restored=1 VDF client stopped:",
    );
    garbage.extend_from_slice(&[b'y'; 2000]);

    let result = tokio::time::timeout(Duration::from_secs(2), diagnostics(&garbage, 333))
        .await
        .expect("unterminated oversized line must not grow without bound");
    assert_eq!(result, (false, Vec::new()));

    let mut bytes = garbage;
    bytes.push(b'\n');
    bytes.extend_from_slice(MARKER.as_bytes());
    bytes.push(b'\n');
    let (confirmed, records) = diagnostics(&bytes, 1024).await;
    assert!(confirmed);
    assert_eq!(records, vec![MARKER.to_string()]);
}

#[tokio::test]
async fn the_diagnostic_cap_holds_and_never_blocks_marker_confirmation() {
    let mut text = String::new();
    for i in 0..9 {
        text.push_str(&format!("safe_readback {i}\n"));
    }
    let (confirmed, records) = diagnostics(text.as_bytes(), 3).await;
    assert!(!confirmed);
    assert_eq!(records.len(), 8);
    assert_eq!(records[7], "safe_readback 7");

    let mut text = String::new();
    for i in 0..8 {
        text.push_str(&format!("safe_readback {i}\n"));
    }
    text.push_str(MARKER);
    text.push('\n');
    let (confirmed, records) = diagnostics(text.as_bytes(), 5).await;
    assert!(confirmed);
    assert_eq!(records.len(), 8);
    assert!(!records.iter().any(|record| record == MARKER));
}

#[tokio::test]
async fn line_limit_counts_content_not_lf_or_crlf_framing() {
    let marker_content = |len: usize| {
        let mut content = "x".repeat(len - MARKER.len());
        content.push_str(MARKER);
        content
    };
    for (len, ending, expected) in [
        (4095, "\n", true),
        (4095, "\r\n", true),
        (4096, "\n", true),
        (4096, "\r\n", true),
        (4097, "\n", false),
        (4097, "\r\n", false),
    ] {
        for chunk in [1, 1024, 8192] {
            let text = format!("{}{ending}", marker_content(len));
            let (confirmed, records) = diagnostics(text.as_bytes(), chunk).await;
            assert_eq!(
                confirmed, expected,
                "len={len}, ending={ending:?}, chunk={chunk}"
            );
            if expected {
                assert_eq!(records.len(), 1);
                assert!(records[0].ends_with(MARKER));
            } else {
                assert!(
                    records.is_empty(),
                    "an oversized line was logged as a diagnostic: {records:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_cancelled_oversized_record_keeps_its_discard_state_through_handoff() {
    let (mut writer, reader) = tokio::io::duplex(16 * 1024);
    let mut lines = Lines::new(BufReader::new(reader));
    writer.write_all(&vec![b'x'; 4097]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), lines.next_line())
            .await
            .is_err()
    );
    // The suffix starts with a space but ends in the marker; losing `discarding`
    // would re-frame it as a fresh line and confirm.
    writer
        .write_all(format!(" {MARKER}\n").as_bytes())
        .await
        .unwrap();
    drop(writer);
    let observed = AtomicBool::new(false);
    let (confirmed, records) = drain_shutdown_diagnostics(&mut lines, &observed)
        .await
        .unwrap();
    assert!(!confirmed);
    assert!(!observed.load(Ordering::Acquire));
    assert!(
        records.is_empty(),
        "a discarded suffix was logged: {records:?}"
    );
}

#[tokio::test]
async fn invalid_utf8_diagnostic_before_the_marker_does_not_hide_confirmation() {
    let mut bytes = b"safe_readback \xff\xfe bad\n".to_vec();
    bytes.extend_from_slice(MARKER.as_bytes());
    bytes.push(b'\n');
    let (confirmed, records) = diagnostics(&bytes, 5).await;
    assert!(confirmed);
    assert_eq!(records, vec![MARKER.to_string()]);
}

#[tokio::test]
async fn unterminated_diagnostic_at_eof_is_logged_without_confirmation() {
    let (confirmed, records) = diagnostics(b"safe_readback last words", 4).await;
    assert!(!confirmed);
    assert_eq!(records, vec!["safe_readback last words".to_string()]);
}

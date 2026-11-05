//! Cancellation-safe native logs: at most 4096 content bytes plus one provisional CR framing byte.
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
const MAX_LINE: usize = 4096;
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Text(String),
    Oversized,
    /// Not valid UTF-8; discarded like an oversized line.
    Invalid,
}
pub struct Lines<R> {
    reader: R,
    pending: Vec<u8>,
    discarding: bool,
}
impl<R: AsyncBufRead + Unpin> Lines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::with_capacity(MAX_LINE + 1),
            discarding: false,
        }
    }
    pub fn into_inner(self) -> R {
        self.reader
    }
    pub async fn next_line(&mut self) -> std::io::Result<Option<Line>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if std::mem::take(&mut self.discarding) || self.pending.len() > MAX_LINE {
                    self.pending.clear();
                    return Ok(Some(Line::Oversized));
                }
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Self::text(std::mem::take(&mut self.pending));
            }
            let end = available.iter().position(|b| *b == b'\n');
            let count = end.unwrap_or(available.len());
            // LF is framing, and a trailing CR may become CRLF even when the
            // next read contains its LF. Retain at most that one extra byte.
            let last = available[..count].last().or_else(|| self.pending.last());
            let limit = MAX_LINE + usize::from(last == Some(&b'\r'));
            if self.pending.len() + count > limit {
                self.pending.clear();
                self.discarding = true;
            }
            if !self.discarding {
                self.pending.extend_from_slice(&available[..count]);
            }
            self.reader.consume(count + usize::from(end.is_some()));
            if end.is_some() {
                if std::mem::take(&mut self.discarding) {
                    return Ok(Some(Line::Oversized));
                }
                let mut bytes = std::mem::take(&mut self.pending);
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                return Self::text(bytes);
            }
            if self.discarding {
                // A log flood must yield to control/proof work. Discard state
                // survives cancellation, so no suffix becomes fresh telemetry.
                tokio::task::yield_now().await;
            }
        }
    }
    fn text(bytes: Vec<u8>) -> std::io::Result<Option<Line>> {
        Ok(Some(
            String::from_utf8(bytes).map_or(Line::Invalid, Line::Text),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};
    #[tokio::test]
    async fn line_limit_counts_content_not_lf_or_crlf_framing() {
        for content in [MAX_LINE, MAX_LINE + 1] {
            for ending in [b"".as_slice(), b"\n".as_slice(), b"\r\n".as_slice()] {
                for capacity in [1, 511, MAX_LINE + 2] {
                    let mut bytes = vec![b'x'; content];
                    bytes.extend_from_slice(ending);
                    let mut lines =
                        Lines::new(BufReader::with_capacity(capacity, bytes.as_slice()));
                    let expected = if content == MAX_LINE {
                        Line::Text("x".repeat(content))
                    } else {
                        Line::Oversized
                    };
                    assert_eq!(
                        lines.next_line().await.unwrap(),
                        Some(expected),
                        "content={content}, ending={ending:?}, buffer={capacity}"
                    );
                    assert!(lines.pending.len() <= MAX_LINE + 1);
                    assert_eq!(lines.next_line().await.unwrap(), None);
                }
            }
        }
        // CR only becomes framing when followed by LF; at EOF it is content.
        let mut bytes = vec![b'x'; MAX_LINE];
        bytes.push(b'\r');
        let mut lines = Lines::new(BufReader::with_capacity(MAX_LINE + 1, bytes.as_slice()));
        assert_eq!(lines.next_line().await.unwrap(), Some(Line::Oversized));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn cancellation_preserves_partial_lines() {
        let (mut sender, receiver) = tokio::io::duplex(8192);
        let mut lines = Lines::new(BufReader::new(receiver));
        sender.write_all(b"partial").await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), lines.next_line())
                .await
                .is_err()
        );
        sender.write_all(b" line\r\n").await.unwrap();
        assert_eq!(
            lines.next_line().await.unwrap(),
            Some(Line::Text("partial line".into()))
        );
    }
    #[tokio::test]
    async fn cancelled_oversized_line_is_discarded_through_its_delimiter() {
        let (mut sender, receiver) = tokio::io::duplex(8192);
        let mut lines = Lines::new(BufReader::new(receiver));
        sender.write_all(&vec![b'x'; MAX_LINE + 1]).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), lines.next_line())
                .await
                .is_err()
        );
        assert!(lines.discarding);
        assert!(lines.pending.len() <= MAX_LINE);
        sender
            .write_all(b"ASIC Temp = 99 C; Frequency = 1450 MHz\nvalid\n")
            .await
            .unwrap();
        assert_eq!(lines.next_line().await.unwrap(), Some(Line::Oversized));
        assert_eq!(
            lines.next_line().await.unwrap(),
            Some(Line::Text("valid".into()))
        );
        drop(sender);
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
    #[tokio::test]
    async fn invalid_utf8_line_is_discarded_and_reading_continues() {
        let bytes = b"bad \xff line\nvalid\r\n\xfe";
        let mut lines = Lines::new(BufReader::new(bytes.as_slice()));
        assert_eq!(lines.next_line().await.unwrap(), Some(Line::Invalid));
        assert_eq!(
            lines.next_line().await.unwrap(),
            Some(Line::Text("valid".into()))
        );
        assert_eq!(lines.next_line().await.unwrap(), Some(Line::Invalid));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
    #[tokio::test]
    async fn oversized_unterminated_eof_is_bounded_and_reported_once() {
        let bytes = vec![b'x'; MAX_LINE * 3];
        let mut lines = Lines::new(BufReader::with_capacity(512, bytes.as_slice()));
        assert_eq!(lines.next_line().await.unwrap(), Some(Line::Oversized));
        assert!(lines.pending.len() <= MAX_LINE);
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
}

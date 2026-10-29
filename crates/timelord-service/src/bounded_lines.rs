//! Cancellation-safe native log framing with a fixed maximum retained line.
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
const MAX_LINE: usize = 4096;
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Text(String),
    Oversized,
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
            pending: Vec::with_capacity(MAX_LINE),
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
                if std::mem::take(&mut self.discarding) {
                    return Ok(Some(Line::Oversized));
                }
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Self::text(std::mem::take(&mut self.pending));
            }
            let end = available.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let count = end.unwrap_or(available.len());
            if self.pending.len() + count > MAX_LINE {
                self.pending.clear();
                self.discarding = true;
            }
            if !self.discarding {
                self.pending.extend_from_slice(&available[..count]);
            }
            self.reader.consume(count);
            if end.is_some() {
                if std::mem::take(&mut self.discarding) {
                    return Ok(Some(Line::Oversized));
                }
                let mut bytes = std::mem::take(&mut self.pending);
                bytes.pop();
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
        String::from_utf8(bytes)
            .map(|line| Some(Line::Text(line)))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};
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
    async fn oversized_unterminated_eof_is_bounded_and_reported_once() {
        let bytes = vec![b'x'; MAX_LINE * 3];
        let mut lines = Lines::new(BufReader::with_capacity(512, bytes.as_slice()));
        assert_eq!(lines.next_line().await.unwrap(), Some(Line::Oversized));
        assert!(lines.pending.len() <= MAX_LINE);
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
}

//! Cancellation-safe native log framing with a fixed maximum retained line.
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
pub struct Lines<R> {
    reader: R,
    pending: Vec<u8>,
}
impl<R: AsyncBufRead + Unpin> Lines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::with_capacity(4096),
        }
    }
    pub fn into_inner(self) -> R {
        self.reader
    }
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return String::from_utf8(std::mem::take(&mut self.pending))
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
            let end = available.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let count = end.unwrap_or(available.len());
            if self.pending.len() + count > 4096 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Native log line exceeds 4096 bytes",
                ));
            }
            self.pending.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if end.is_some() {
                let mut bytes = std::mem::take(&mut self.pending);
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                return String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};
    #[tokio::test]
    async fn cancellation_preserves_partial_lines_and_oversize_is_rejected() {
        let (mut sender, receiver) = tokio::io::duplex(8192);
        let mut lines = Lines::new(BufReader::new(receiver));
        sender.write_all(b"partial").await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), lines.next_line())
                .await
                .is_err()
        );
        sender.write_all(b" line\r\n").await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "partial line");
        sender.write_all(&vec![b'x'; 4097]).await.unwrap();
        assert!(lines.next_line().await.is_err());
        assert!(lines.pending.len() <= 4096);
    }
}

use tokio::io::{AsyncRead, ReadBuf};

/// A reader that hands out at most `chunk` bytes per poll.
pub(crate) struct Chunked<'a> {
    pub bytes: &'a [u8],
    pub chunk: usize,
}

impl AsyncRead for Chunked<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let count = self.bytes.len().min(self.chunk).min(output.remaining());
        output.put_slice(&self.bytes[..count]);
        self.bytes = &self.bytes[count..];
        std::task::Poll::Ready(Ok(()))
    }
}

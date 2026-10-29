//! Bound connection lifetimes and retained response bodies, including slow readers.
use axum::{
    Router,
    body::{Body, HttpBody},
    http::{Request, StatusCode},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Sleep,
};

pub struct Listener {
    inner: TcpListener,
    slots: Arc<Semaphore>,
}
pub struct Connection {
    inner: TcpStream,
    _slot: OwnedSemaphorePermit,
    deadline: Pin<Box<Sleep>>,
}
impl Listener {
    pub fn new(inner: TcpListener) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(64)),
        }
    }
}
impl axum::serve::Listener for Listener {
    type Io = Connection;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Connection, Self::Addr) {
        // Await capacity before accepting. Dropping a stream and immediately
        // retrying a ready backlog can otherwise monopolize the runtime.
        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("listener slots remain open");
        loop {
            match self.inner.accept().await {
                Ok((inner, address)) => {
                    let _ = inner.set_nodelay(true);
                    return (
                        Connection {
                            inner,
                            _slot: slot,
                            deadline: Box::pin(tokio::time::sleep(Duration::from_secs(60))),
                        },
                        address,
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
impl Connection {
    fn live(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.deadline.as_mut().poll(cx).is_ready() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Diagnostic connection lifetime exceeded",
            ))
        } else {
            Ok(())
        }
    }
}
impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.live(cx)?;
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}
impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.live(cx)?;
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.live(cx)?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
pub fn protect<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    let small = Arc::new(Semaphore::new(16));
    let history = Arc::new(Semaphore::new(4));
    router.layer(from_fn(move |request: Request<Body>, next: Next| {
        let slots = if request.uri().path() == "/v3/history" {
            history.clone()
        } else {
            small.clone()
        };
        async move {
            let Ok(permit) = slots.try_acquire_owned() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let response =
                match tokio::time::timeout(Duration::from_secs(5), next.run(request)).await {
                    Ok(response) => response,
                    Err(_) => return StatusCode::GATEWAY_TIMEOUT.into_response(),
                };
            let (mut parts, body) = response.into_parts();
            // Preserve exact framing for the bounded native HTTP reader.
            if let Some(size) = body.size_hint().exact() {
                parts
                    .headers
                    .entry(axum::http::header::CONTENT_LENGTH)
                    .or_insert_with(|| size.to_string().parse().expect("decimal content length"));
            }
            let stream = futures_util::stream::unfold(
                (body.into_data_stream(), permit),
                |(mut source, permit)| async move {
                    use futures_util::StreamExt;
                    source.next().await.map(|part| (part, (source, permit)))
                },
            );
            Response::from_parts(parts, Body::from_stream(stream))
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::serve::Listener as _;
    use tokio::io::AsyncReadExt;
    #[tokio::test]
    async fn idle_connections_expire_and_excess_connections_wait_without_spinning() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let mut listener = Listener {
            inner: tcp,
            slots: slots.clone(),
        };
        let _client = TcpStream::connect(addr).await.unwrap();
        let (mut accepted, _) = listener.accept().await;
        assert_eq!(slots.available_permits(), 0);
        let mut excess = TcpStream::connect(addr).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        let mut bytes = [0; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), excess.read(&mut bytes))
                .await
                .is_err()
        );
        accepted.deadline = Box::pin(tokio::time::sleep(Duration::from_millis(20)));
        assert_eq!(
            accepted.read(&mut bytes).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        drop(accepted);
        assert_eq!(slots.available_permits(), 1);
        let (mut queued, _) = listener.accept().await;
        use tokio::io::AsyncWriteExt;
        excess.write_all(b"x").await.unwrap();
        queued.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, [b'x']);
    }
}

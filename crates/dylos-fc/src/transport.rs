use std::io;
use std::pin::Pin;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

#[derive(Default)]
pub(crate) struct RequestState {
    pub(crate) error: OnceLock<Arc<io::Error>>,
    pub(crate) sent: AtomicBool,
}

/// Preserve an early HTTP response when the peer closes its request reader.
/// After EPIPE, remaining request bytes are discarded and reported as written only
/// to parse the error response; callers must reject non-error responses.
/// Hyper otherwise stops on EPIPE before parsing already-received response bytes.
pub(crate) struct ResponseStream {
    stream: UnixStream,
    first_byte: Option<u8>,
    write_closed: bool,
    state: Arc<RequestState>,
    remaining_body: usize,
    header_tail: u32,
    headers_sent: bool,
    response_started: bool,
}

impl ResponseStream {
    pub(crate) fn new(stream: UnixStream, state: Arc<RequestState>, body_len: usize) -> Self {
        Self {
            stream,
            first_byte: None,
            write_closed: false,
            state,
            remaining_body: body_len,
            header_tail: 0,
            headers_sent: false,
            response_started: false,
        }
    }

    // Requests use Full JSON bodies with known lengths, never chunked encoding.
    // Count only real socket writes, excluding bytes discarded after EPIPE.
    fn record_write(&mut self, bytes: &[u8]) {
        let mut body_start = 0;
        if !self.headers_sent {
            for (index, byte) in bytes.iter().enumerate() {
                self.header_tail = (self.header_tail << 8) | u32::from(*byte);
                if self.header_tail == 0x0d0a_0d0a {
                    self.headers_sent = true;
                    body_start = index + 1;
                    break;
                }
            }
            if !self.headers_sent {
                return;
            }
        }
        self.remaining_body = self.remaining_body.saturating_sub(bytes.len() - body_start);
        if self.remaining_body == 0 {
            self.state.sent.store(true, Ordering::Release);
        }
    }
}

impl AsyncRead for ResponseStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() > 0
            && let Some(byte) = self.first_byte.take()
        {
            buf.put_slice(&[byte]);
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        self.response_started |= buf.filled().len() > before;
        result
    }
}

impl AsyncWrite for ResponseStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.write_closed {
            return Poll::Ready(Ok(buf.len()));
        }
        if self.state.error.get().is_none() {
            match Pin::new(&mut self.stream).poll_write(cx, buf) {
                Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                    self.state.error.get_or_init(|| Arc::new(error));
                }
                Poll::Ready(Ok(n)) => {
                    self.record_write(&buf[..n]);
                    return Poll::Ready(Ok(n));
                }
                result => return result,
            }
        }
        if self.response_started {
            self.write_closed = true;
            return Poll::Ready(Ok(buf.len()));
        }
        let mut byte = [0];
        let mut read_buf = ReadBuf::new(&mut byte);
        // Read readiness may not have been registered when EPIPE is reported.
        // Wait for it rather than treating WouldBlock as an absent response.
        let read = Pin::new(&mut self.stream).poll_read(cx, &mut read_buf);
        if read.is_pending() {
            tracing::debug!(
                waiting_for_early_response = true,
                "Request not fully sent; awaiting early response after EPIPE"
            );
            return Poll::Pending;
        }
        if let Some(error) = self.state.error.get() {
            if matches!(read, Poll::Ready(Ok(()))) && !read_buf.filled().is_empty() {
                tracing::debug!(%error, "Peer closed request reader with a response pending");
                self.first_byte = Some(byte[0]);
                self.write_closed = true;
                return Poll::Ready(Ok(buf.len()));
            }
            if let Poll::Ready(Err(read_error)) = read {
                tracing::debug!(%read_error, "No early response available after EPIPE");
            }
            return Poll::Ready(Err(io::Error::new(error.kind(), error.to_string())));
        }
        Poll::Ready(Err(io::Error::other("missing deferred write error")))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

/// Preserve an early HTTP response when the peer closes its request reader.
/// Hyper otherwise stops on EPIPE before parsing already-received response bytes.
pub(crate) struct ResponseStream {
    stream: UnixStream,
    first_byte: Option<u8>,
    write_closed: bool,
    write_error: Option<io::Error>,
}

impl ResponseStream {
    pub(crate) fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            first_byte: None,
            write_closed: false,
            write_error: None,
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
        Pin::new(&mut self.stream).poll_read(cx, buf)
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
        if self.write_error.is_none() {
            match Pin::new(&mut self.stream).poll_write(cx, buf) {
                Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                    self.write_error = Some(error);
                }
                result => return result,
            }
        }
        let mut byte = [0];
        let mut read_buf = ReadBuf::new(&mut byte);
        // Read readiness may not have been registered when EPIPE is reported.
        // Wait for it rather than treating WouldBlock as an absent response.
        let read = Pin::new(&mut self.stream).poll_read(cx, &mut read_buf);
        if read.is_pending() {
            return Poll::Pending;
        }
        if let Some(error) = self.write_error.take() {
            if matches!(read, Poll::Ready(Ok(()))) && !read_buf.filled().is_empty() {
                tracing::debug!(%error, "Peer closed request reader with a response pending");
                self.first_byte = Some(byte[0]);
                self.write_closed = true;
                return Poll::Ready(Ok(buf.len()));
            }
            if let Poll::Ready(Err(read_error)) = read {
                tracing::debug!(%read_error, "No early response available after EPIPE");
            }
            return Poll::Ready(Err(error));
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

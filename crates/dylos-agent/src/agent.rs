use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::clock::{ClockError, GuestClock};
use crate::protocol::{MAX_LINE_BYTES, Reply, ReplyBody, Request, RequestBody};

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("connection I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("request line exceeds {MAX_LINE_BYTES} bytes")]
    LineTooLong,
    #[error("cannot encode reply: {0}")]
    Encode(#[from] crate::protocol::ProtocolError),
}

/// Reconnection delay: starts at `initial`, doubles after each failed
/// attempt and is capped at `max`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(5),
        }
    }
}

impl Backoff {
    #[must_use]
    pub fn next(&self, current: Duration) -> Duration {
        current.saturating_mul(2).min(self.max)
    }
}

/// Answers requests on `stream` until the peer closes it.
///
/// A bad request gets an `error` reply and the connection stays open.
///
/// # Errors
/// Fails on I/O errors or an oversized request line.
pub async fn serve<S, C>(stream: S, clock: &C) -> Result<(), ServeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: GuestClock,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_LINE_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(());
        }
        if line.last() != Some(&b'\n') {
            if read < MAX_LINE_BYTES {
                // Peer closed in the middle of a line.
                return Ok(());
            }
            return Err(ServeError::LineTooLong);
        }
        let reply = match std::str::from_utf8(&line) {
            Ok(text) => handle_line(text, clock),
            Err(error) => error_reply(format!("request is not UTF-8: {error}")),
        };
        write_half.write_all(reply.to_line()?.as_bytes()).await?;
        write_half.flush().await?;
    }
}

fn handle_line<C: GuestClock>(line: &str, clock: &C) -> Reply {
    match Request::from_line(line) {
        Ok(request) => handle(&request.body, clock),
        Err(error) => error_reply(error.to_string()),
    }
}

fn handle<C: GuestClock>(body: &RequestBody, clock: &C) -> Reply {
    let result = match body {
        RequestBody::Resync { unix_time_ns } => clock.set_unix_ns(*unix_time_ns).map(|()| {
            info!(unix_time_ns = *unix_time_ns, "guest clock resynchronized");
            ReplyBody::Ok
        }),
        RequestBody::Health => health(clock),
    };
    match result {
        Ok(body) => Reply::new(body),
        Err(error) => error_reply(error.to_string()),
    }
}

fn health<C: GuestClock>(clock: &C) -> Result<ReplyBody, ClockError> {
    Ok(ReplyBody::Health {
        uptime_ms: u64::try_from(clock.uptime()?.as_millis()).unwrap_or(u64::MAX),
        wall_clock_unix_ns: clock.now_unix_ns()?,
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

fn error_reply(message: String) -> Reply {
    warn!(%message, "request rejected");
    Reply::new(ReplyBody::Error { message })
}

/// Connects with `connect`, serves until the connection drops, and repeats forever.
///
/// vsock connections are reset when a VM is restored from a snapshot
/// (ADR-0001), so the agent has to dial the host again by itself.
pub async fn run<S, C, F, Fut>(mut connect: F, clock: &C, backoff: Backoff) -> Infallible
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: GuestClock,
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<S>>,
{
    let mut delay = backoff.initial;
    loop {
        match connect().await {
            Ok(stream) => {
                info!("connected to host");
                delay = backoff.initial;
                match serve(stream, clock).await {
                    Ok(()) => info!("host closed the connection"),
                    Err(error) => warn!(%error, "connection lost"),
                }
            }
            Err(error) => {
                debug!(%error, retry_in = ?delay, "cannot connect to host");
                delay = backoff.next(delay);
            }
        }
        tokio::time::sleep(delay).await;
    }
}

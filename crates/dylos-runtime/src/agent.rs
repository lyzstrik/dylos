//! Bounded host endpoint for the untrusted guest agent.
use std::time::{Duration, SystemTime};

use dylos_agent::protocol::{MAX_LINE_BYTES, Reply, ReplyBody, Request, RequestBody};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("agent exchange timed out")]
    Timeout,
    #[error("agent line exceeds {MAX_LINE_BYTES} bytes or is incomplete")]
    Line,
    #[error("agent reply is not UTF-8")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid agent protocol: {0}")]
    Protocol(#[from] dylos_agent::protocol::ProtocolError),
    #[error("unexpected agent reply")]
    Unexpected,
    #[error("host time is outside the protocol range")]
    Clock,
}

async fn reply<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Reply, AgentError> {
    let mut line = [0; MAX_LINE_BYTES];
    for end in 0..MAX_LINE_BYTES {
        stream.read_exact(&mut line[end..=end]).await?;
        if line[end] == b'\n' {
            return Ok(Reply::from_line(std::str::from_utf8(&line[..=end])?)?);
        }
    }
    Err(AgentError::Line)
}

async fn request<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    body: RequestBody,
) -> Result<ReplyBody, AgentError> {
    stream
        .write_all(Request::new(body).to_line()?.as_bytes())
        .await?;
    Ok(reply(stream).await?.body)
}

/// Resynchronizes wall time and reads health over an already accepted connection.
/// The deadline covers writes and all guest reads, including partial lines.
/// No buffer size depends on guest data.
///
/// # Errors
/// Rejects oversized, malformed, unsupported or unexpected replies and stalled peers.
pub async fn resync<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    deadline: Duration,
) -> Result<ReplyBody, AgentError> {
    tokio::time::timeout(deadline, async {
        let unix_time_ns = u64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| AgentError::Clock)?
                .as_nanos(),
        )
        .map_err(|_| AgentError::Clock)?;
        if request(stream, RequestBody::Resync { unix_time_ns }).await? != ReplyBody::Ok {
            return Err(AgentError::Unexpected);
        }
        match request(stream, RequestBody::Health).await? {
            health @ ReplyBody::Health { .. } => Ok(health),
            _ => Err(AgentError::Unexpected),
        }
    })
    .await
    .map_err(|_| AgentError::Timeout)?
}

pub(crate) fn listeners(
    vms: &[crate::Vm],
    config: &crate::jailer::JailerConfig,
) -> Result<Vec<tokio::net::UnixListener>, std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    vms.iter()
        .map(|vm| {
            let path = vm
                .paths()
                .root
                .join(format!("vsock.sock_{}", dylos_agent::AGENT_VSOCK_PORT));
            let listener = tokio::net::UnixListener::bind(&path)?;
            nix::unistd::chown(
                &path,
                Some(nix::unistd::Uid::from_raw(config.uid)),
                Some(nix::unistd::Gid::from_raw(config.gid)),
            )?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(listener)
        })
        .collect()
}

pub(crate) async fn resync_all(
    vms: &[crate::Vm],
    listeners: &[tokio::net::UnixListener],
) -> Result<(), AgentError> {
    let results =
        futures_util::future::join_all(vms.iter().zip(listeners).map(|(vm, listener)| async {
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_secs(15), async {
                let (mut stream, _) = listener.accept().await?;
                resync(&mut stream, Duration::from_secs(5)).await?;
                Ok::<_, AgentError>(())
            })
            .await
            .map_err(|_| AgentError::Timeout)?;
            tracing::info!(
                vm = vm.node(),
                duration_ms = started.elapsed().as_millis(),
                success = result.is_ok(),
                "guest clock resync completed"
            );
            result
        }))
        .await;
    for result in results {
        result?;
    }
    Ok(())
}

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("VM {id}: process exited while waiting for readiness")]
    ReadinessExit { id: String },
    #[error("VM {id}: readiness failed: {line}")]
    ReadinessFailed { id: String, line: String },
    #[error("VM {id}: readiness not reached after {timeout:?}")]
    ReadinessTimeout { id: String, timeout: Duration },
    #[error("invalid jail id {id:?}: must be 1 to 64 of [A-Za-z0-9-]")]
    InvalidJailId { id: String },
    #[error("invalid chroot file name {name:?} for VM {id}: must be a single path component")]
    InvalidChrootFileName { id: String, name: String },
    #[error("jail directory {path:?} already exists: stale VM {id} or VM still running")]
    JailExists { id: String, path: PathBuf },
    #[error("VM {id}: {op} failed on {path:?}")]
    Io {
        id: String,
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("VM {id}: failed to spawn launcher {program:?}")]
    Spawn {
        id: String,
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("VM {id}: process exited during start ({state:?})\n{output}")]
    ExitedDuringStart {
        id: String,
        state: crate::vm::ProcessState,
        output: String,
    },
    #[error(
        "VM {id}: API socket {socket:?} not ready after {timeout:?}; last probe error: {last_probe_error:?}\n{output}"
    )]
    ReadyTimeout {
        id: String,
        socket: PathBuf,
        timeout: Duration,
        last_probe_error: Option<String>,
        output: String,
    },
    #[error("VM {id}: {task} task failed")]
    Task {
        id: String,
        task: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("VM {id}: cleanup did not complete within {timeout:?}")]
    CleanupTimeout { id: String, timeout: Duration },
    #[error("VM {id}: earlier cleanup failed: {message}")]
    CleanupFailed { id: String, message: String },
    #[error("VM {id}: metrics_poll must not be zero")]
    ZeroMetricsPoll { id: String },
    #[error("VM {id}: process still alive {timeout:?} after SIGKILL")]
    KillTimeout { id: String, timeout: Duration },
}

pub type Result<T> = std::result::Result<T, Error>;

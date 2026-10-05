use std::time::Duration;

use nix::time::{ClockId, clock_gettime, clock_settime};

#[derive(Debug, thiserror::Error)]
#[error("clock operation `{operation}` failed: {source}")]
pub struct ClockError {
    pub operation: &'static str,
    #[source]
    pub source: std::io::Error,
}

pub trait GuestClock {
    /// # Errors
    /// Fails if the kernel rejects the new time (typically missing `CAP_SYS_TIME`).
    fn set_unix_ns(&self, unix_ns: u64) -> Result<(), ClockError>;

    /// # Errors
    /// Fails if the clock cannot be read.
    fn now_unix_ns(&self) -> Result<u64, ClockError>;

    /// Time since boot, including time the guest was paused.
    ///
    /// # Errors
    /// Fails if the clock cannot be read.
    fn uptime(&self) -> Result<Duration, ClockError>;
}

/// The kernel clocks of the machine the agent runs on.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl GuestClock for SystemClock {
    fn set_unix_ns(&self, unix_ns: u64) -> Result<(), ClockError> {
        clock_settime(
            ClockId::CLOCK_REALTIME,
            nix::sys::time::TimeSpec::from_duration(Duration::from_nanos(unix_ns)),
        )
        .map_err(|errno| ClockError {
            operation: "clock_settime(CLOCK_REALTIME)",
            source: errno.into(),
        })
    }

    fn now_unix_ns(&self) -> Result<u64, ClockError> {
        let now = read(ClockId::CLOCK_REALTIME, "clock_gettime(CLOCK_REALTIME)")?;
        u64::try_from(now.as_nanos()).map_err(|_| ClockError {
            operation: "clock_gettime(CLOCK_REALTIME)",
            source: std::io::Error::other("wall clock does not fit in u64 nanoseconds"),
        })
    }

    fn uptime(&self) -> Result<Duration, ClockError> {
        read(ClockId::CLOCK_BOOTTIME, "clock_gettime(CLOCK_BOOTTIME)")
    }
}

fn read(id: ClockId, operation: &'static str) -> Result<Duration, ClockError> {
    let spec = clock_gettime(id).map_err(|errno| ClockError {
        operation,
        source: errno.into(),
    })?;
    Ok(Duration::from(spec))
}

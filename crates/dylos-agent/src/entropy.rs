//! Fresh host entropy mixing. A write alone does not force a kernel CRNG reseed.
use std::io::Write;

use crate::protocol::ENTROPY_BYTES;

/// Injectable entropy operation for unprivileged protocol tests.
pub trait GuestEntropy: Send + Sync {
    /// # Errors
    /// Fails if the guest entropy device cannot be opened or written.
    fn mix(&self, bytes: [u8; ENTROPY_BYTES]) -> std::io::Result<()>;
}

#[derive(Clone, Copy)]
pub struct SystemEntropy;

impl GuestEntropy for SystemEntropy {
    fn mix(&self, bytes: [u8; ENTROPY_BYTES]) -> std::io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/urandom")?
            .write_all(&bytes)
    }
}

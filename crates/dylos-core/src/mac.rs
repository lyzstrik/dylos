use sha2::{Digest, Sha256};
use std::fmt;

/// A MAC address. Knows nothing about labs: callers choose the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    /// Always locally administered unicast: first byte `02`, then the first 5 bytes of the
    /// SHA-256 of the seed, so the same seed gives the same MAC on every boot and clone.
    #[must_use]
    pub fn from_seed(seed: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(seed);
        let result = hasher.finalize();

        Self([0x02, result[0], result[1], result[2], result[3], result[4]])
    }

    #[must_use]
    pub fn is_locally_administered(&self) -> bool {
        self.0[0] & 0b0000_0010 != 0
    }

    #[must_use]
    pub fn is_unicast(&self) -> bool {
        self.0[0] & 0b0000_0001 == 0
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

use sha2::{Digest, Sha256};
use std::fmt;

/// A deterministic MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    /// Derives a deterministic MAC address from a seed.
    ///
    /// The generated MAC address is always a locally administered unicast address.
    /// The first byte is set to `02`, and the remaining 5 bytes are taken from the
    /// SHA-256 hash of the seed.
    #[must_use]
    pub fn from_seed(seed: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(seed);
        let result = hasher.finalize();

        // 02 is locally administered unicast
        Self([0x02, result[0], result[1], result[2], result[3], result[4]])
    }

    /// Returns true if the MAC address is locally administered.
    #[must_use]
    pub fn is_locally_administered(&self) -> bool {
        self.0[0] & 0b0000_0010 != 0
    }

    /// Returns true if the MAC address is unicast.
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

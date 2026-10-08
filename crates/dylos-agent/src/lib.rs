//! In-guest agent over vsock.
//!
//! This crate contains the agent binary that runs inside the guest VMs.
//! It is built statically (musl) and is completely standalone. It does not
//! depend on any other Dylos crate. See [`protocol`] for the wire format.
#![forbid(unsafe_code)]

pub mod agent;
pub mod clock;
pub mod protocol;

/// vsock port the agent dials on the host (CID 2).
///
/// With Firecracker, a guest connection to this port arrives on the Unix
/// socket `<vsock uds_path>_<port>` on the host.
pub const AGENT_VSOCK_PORT: u32 = 5000;

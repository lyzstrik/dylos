#![forbid(unsafe_code)]

//! In-guest agent: keeps a vsock connection to the host and answers
//! `resync` and `health` requests. See [`protocol`] for the wire format.

pub mod agent;
pub mod clock;
pub mod protocol;

/// vsock port the agent dials on the host (CID 2).
///
/// With Firecracker, a guest connection to this port arrives on the Unix
/// socket `<vsock uds_path>_<port>` on the host.
pub const AGENT_VSOCK_PORT: u32 = 5000;

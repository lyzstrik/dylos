//! Orchestration: up, down, snapshot, restore, and fork sequences.
//!
//! # Current status of the spike
//!
//! The runtime launches labs and supervises their VMs until teardown.
//! Lab-wide features such as lab-wide snapshot, restore, or fork do not exist yet (LYZ-19, LYZ-22, LYZ-23).
//! Fabric features such as fabric freeze/thaw do not exist yet (LYZ-24).
//!
//! # Target Design Constraints
//!
//! This crate is designed to implement the high-level workflows for lab lifecycles.
//! It depends on `dylos-fc`, `dylos-net`, and `dylos-store`, but must never depend
//! on the CLI application (`dylos-cli`).
#![forbid(unsafe_code)]

pub mod agent;
pub mod error;
pub mod fleet;
pub mod jailer;
pub mod lab;
pub mod pause;
pub mod snapshot;
pub mod vm;

pub use error::{Error, Result};
pub use fleet::{Fleet, FleetError, LabFailure};
pub use jailer::{MAX_UNIX_SOCKET_PATH, SUN_PATH_LEN, validate_unix_socket_path};
pub use lab::{Lab, LabConfig, LabError};
pub use pause::{pause_all, resume_all};
pub use snapshot::{SnapshotRequest, SnapshotSlot, SnapshotTimings, snapshot_vms, verify_snapshot};
pub use vm::{ExecOutput, ProcessState, Timeouts, Vm, VmSpec, validate_resources};

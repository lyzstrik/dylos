//! Network namespaces, bridges, TAP devices, and fabric freeze/thaw.
//!
//! # Current status of the spike
//!
//! Freeze takes every TAP down. Frames sent after freeze completes are rejected, including
//! across thaw; earlier queued frames may still be read. Their send precedes every snapshot,
//! so they preserve the consistent cut. Clones have fresh TAPs without those host queues.
//! This implements the revised invariant authorized for LYZ-19 (ADR revision tracked in LYZ-53).
//! Firecracker device buffers (including deferred RX in `Net::prepare_save`) and guest/virtio
//! receive counters remain unverified until the real-VM test after LYZ-52.
//!
//! # Target Design Constraints
//!
//! This crate manages the host network resources required by a lab.
//! It depends on `dylos-core` for lab definitions, but must never depend on
//! higher-level orchestration crates (like `dylos-runtime` or `dylos-cli`).

mod error;
mod fabric;
mod freeze;
mod netns;
mod plan;
mod tap;

pub use error::Error;
pub use fabric::{LabNetwork, teardown};
pub use netns::DEFAULT_NETNS_DIR;
pub use plan::{Bridge, FabricPlan, IFNAME_MAX_LEN, Tap};

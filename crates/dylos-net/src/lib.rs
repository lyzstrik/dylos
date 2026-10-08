//! Network namespaces, bridges, TAP devices, and fabric freeze/thaw.
//!
//! # Current status of the spike
//!
//! Fabric features such as fabric freeze/thaw do not exist yet (LYZ-24).
//!
//! # Target Design Constraints
//!
//! This crate manages the host network resources required by a lab.
//! It depends on `dylos-core` for lab definitions, but must never depend on
//! higher-level orchestration crates (like `dylos-runtime` or `dylos-cli`).

mod error;
mod fabric;
mod netns;
mod plan;
mod tap;

pub use error::Error;
pub use fabric::{LabNetwork, teardown};
pub use netns::DEFAULT_NETNS_DIR;
pub use plan::{Bridge, FabricPlan, IFNAME_MAX_LEN, Tap};

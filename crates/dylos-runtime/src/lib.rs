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

pub mod error;
pub mod jailer;
pub mod lab;
pub mod vm;

pub use error::{Error, Result};
pub use lab::{Lab, LabConfig, LabError};
pub use vm::{ProcessState, Timeouts, Vm, VmSpec};

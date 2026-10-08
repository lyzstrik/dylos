//! Orchestration: up, down, snapshot, restore, and fork sequences.
//!
//! This crate implements the high-level workflows for lab lifecycles.
//! It depends on `dylos-fc`, `dylos-net`, and `dylos-store`, but must never depend
//! on the CLI application (`dylos-cli`).
#![forbid(unsafe_code)]

pub mod error;
pub mod jailer;
pub mod vm;

pub use error::{Error, Result};
pub use vm::{ProcessState, Timeouts, Vm, VmSpec};

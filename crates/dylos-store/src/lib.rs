//! Lab directory layout and reflink cloning of disks.
//!
//! # Current status of the spike
//!
//! `dylos-store` is currently being written (LYZ-50).
//!
//! # Target Design Constraints
//!
//! This crate manages the host directory structure for a lab and provides fast
//! copy-on-write disk cloning via `ioctl_ficlone`.
//! It depends on `dylos-core`, but must never depend on higher-level orchestration
//! crates (like `dylos-runtime` or `dylos-cli`).

//! Typed client for the Firecracker HTTP API over a Unix socket.
//!
//! This crate provides the API bindings to interact with a Firecracker microVM.
//! It depends on `dylos-core` for basic types, but never depends on higher-level
//! orchestration crates (like `dylos-runtime`).
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod error;
pub mod snapshot;

pub use client::FcClient;
pub use error::{Error, Result};

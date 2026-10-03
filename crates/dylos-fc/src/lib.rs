#![forbid(unsafe_code)]

pub mod client;
pub mod error;

pub use client::FcClient;
pub use error::{Error, Result};

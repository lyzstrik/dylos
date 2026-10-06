//! Line-delimited JSON protocol between the host and the in-guest agent.
//!
//! Each message is one JSON object on one line, terminated by `\n`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Longest accepted line, newline included.
pub const MAX_LINE_BYTES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("message has no numeric protocol version")]
    MissingVersion,
    #[error("unsupported protocol version {got}, expected {PROTOCOL_VERSION}")]
    UnsupportedVersion { got: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// Set the guest wall clock to `unix_time_ns` (nanoseconds since the Unix epoch).
    Resync {
        unix_time_ns: u64,
    },
    Health,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplyBody {
    Ok,
    Health {
        uptime_ms: u64,
        wall_clock_unix_ns: u64,
        agent_version: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub v: u32,
    #[serde(flatten)]
    pub body: ReplyBody,
}

impl Request {
    #[must_use]
    pub fn new(body: RequestBody) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body,
        }
    }

    /// Encodes the request as one line including the trailing newline.
    ///
    /// # Errors
    /// Fails if serialization fails.
    pub fn to_line(&self) -> Result<String, ProtocolError> {
        encode(self)
    }

    /// # Errors
    /// Fails on malformed JSON or when the version is not [`PROTOCOL_VERSION`].
    pub fn from_line(line: &str) -> Result<Self, ProtocolError> {
        decode(line)
    }
}

impl Reply {
    #[must_use]
    pub fn new(body: ReplyBody) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body,
        }
    }

    /// Encodes the reply as one line including the trailing newline.
    ///
    /// # Errors
    /// Fails if serialization fails.
    pub fn to_line(&self) -> Result<String, ProtocolError> {
        encode(self)
    }

    /// # Errors
    /// Fails on malformed JSON or when the version is not [`PROTOCOL_VERSION`].
    pub fn from_line(line: &str) -> Result<Self, ProtocolError> {
        decode(line)
    }
}

fn encode<T: Serialize>(message: &T) -> Result<String, ProtocolError> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    Ok(line)
}

fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, ProtocolError> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    // The version is checked before the body so that a future version with
    // unknown message types is reported as a version mismatch.
    let version = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ProtocolError::MissingVersion)?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(ProtocolError::UnsupportedVersion { got: version });
    }
    Ok(serde_json::from_value(value)?)
}

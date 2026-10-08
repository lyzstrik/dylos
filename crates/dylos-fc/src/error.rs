use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("response too large for {method} {route} on {path:?}: exceeded limit of {limit} bytes")]
    ResponseTooLarge {
        path: PathBuf,
        method: String,
        route: String,
        limit: usize,
    },
    #[error("request timed out for {method} {route} on {path:?}")]
    Timeout {
        path: PathBuf,
        method: String,
        route: String,
    },
    #[error("failed to connect to socket {path:?} for {method} {route}")]
    Connect {
        path: PathBuf,
        method: String,
        route: String,
        #[source]
        source: std::io::Error,
    },
    #[error("HTTP handshake failed on {path:?} for {method} {route}")]
    Handshake {
        path: PathBuf,
        method: String,
        route: String,
        #[source]
        source: hyper::Error,
    },
    #[error("HTTP request failed: {method} {route} on {path:?}")]
    Request {
        path: PathBuf,
        method: String,
        route: String,
        #[source]
        source: hyper::Error,
    },
    #[error("API error {status} for {method} {route} on {path:?}: {fault_message}")]
    Api {
        path: PathBuf,
        method: String,
        route: String,
        status: u16,
        fault_message: String,
    },
    #[error("failed to serialize request body for {method} {route} on {path:?}")]
    Serialize {
        path: PathBuf,
        method: String,
        route: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "failed to deserialize response body (status {status}) for {method} {route} on {path:?}"
    )]
    Deserialize {
        path: PathBuf,
        method: String,
        route: String,
        status: u16,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to read response body (status {status}) for {method} {route} on {path:?}")]
    BodyRead {
        path: PathBuf,
        method: String,
        route: String,
        status: u16,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to build request for {method} {route} on {path:?}")]
    RequestBuilder {
        path: PathBuf,
        method: String,
        route: String,
        #[source]
        source: hyper::http::Error,
    },
    #[error("invalid route {route:?}: must start with '/'")]
    InvalidRoute { route: String },
    #[error("invalid URI for route {route:?}")]
    InvalidUri {
        route: String,
        #[source]
        source: hyper::http::uri::InvalidUri,
    },
    #[error("invalid id {id:?}: must be non-empty and contain only [A-Za-z0-9_-]")]
    InvalidId { id: String },
}

pub type Result<T> = std::result::Result<T, Error>;

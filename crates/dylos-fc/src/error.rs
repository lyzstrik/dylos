use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to connect to socket {path:?}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("HTTP handshake failed on {path:?}")]
    Handshake {
        path: PathBuf,
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
    #[error("failed to serialize request body")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to deserialize response body")]
    Deserialize(#[source] serde_json::Error),
    #[error("failed to read response body")]
    BodyRead(#[source] hyper::Error),
    #[error("failed to build request")]
    RequestBuilder(#[source] hyper::http::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

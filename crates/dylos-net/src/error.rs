use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid lab id {0:?}: expected 1 to 64 ASCII letters, digits, '-' or '_'")]
    InvalidLabId(String),

    #[error("{source_desc}: derived interface name {ifname:?} {reason}")]
    InvalidIfName {
        source_desc: String,
        ifname: String,
        reason: &'static str,
    },

    #[error("interface name {ifname:?} is derived from both {first} and {second}")]
    IfNameCollision {
        ifname: String,
        first: String,
        second: String,
    },

    #[error("node {node:?} interface {interface:?}: segment {segment:?} does not exist")]
    UnknownSegment {
        node: String,
        interface: String,
        segment: String,
    },

    #[error("lab {lab}: network namespace {} already exists", path.display())]
    NetnsExists { lab: String, path: PathBuf },

    #[error("lab {lab}: {op} {target}: {source}")]
    Io {
        lab: String,
        op: &'static str,
        target: String,
        source: io::Error,
    },

    #[error("lab {lab}: netlink {op} {link}: {source}")]
    Netlink {
        lab: String,
        op: &'static str,
        link: String,
        source: Box<rtnetlink::Error>,
    },

    #[error("lab {lab}: namespace worker thread panicked")]
    WorkerPanicked { lab: String },
}

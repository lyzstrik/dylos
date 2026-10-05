use ipnet::IpNet;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("parse error: {0}")]
    Parse(#[from] serde_saphyr::Error),

    #[error("{path}: duplicate node name {name:?}")]
    DuplicateNodeName { path: String, name: String },

    #[error("{path}: duplicate segment name {name:?}")]
    DuplicateSegmentName { path: String, name: String },

    #[error("{path}: duplicate interface name {name:?}")]
    DuplicateInterfaceName { path: String, name: String },

    #[error("{path}: {ip} is not inside segment {segment:?} ({cidr})")]
    IpOutsideSegment {
        path: String,
        ip: std::net::IpAddr,
        segment: String,
        cidr: IpNet,
    },

    #[error(
        "{path}: interface prefix /{prefix_len} is wider than segment {segment:?} prefix /{segment_prefix_len}"
    )]
    PrefixWiderThanSegment {
        path: String,
        prefix_len: u8,
        segment: String,
        segment_prefix_len: u8,
    },

    #[error("{path}: interface has ipv4 but segment {segment:?} does not (or vice versa)")]
    Ipv4Mismatch { path: String, segment: String },

    #[error("{path}: duplicate IP {ip}")]
    DuplicateIp { path: String, ip: std::net::IpAddr },

    #[error("{path}: segment {segment:?} does not exist")]
    UnknownSegment { path: String, segment: String },

    #[error(
        "{path}: gateway {gateway} is not reachable on any of the node's connected subnets, or is a node's own address"
    )]
    UnreachableGateway {
        path: String,
        gateway: std::net::IpAddr,
    },

    #[error("{path}: route destination and gateway families do not match")]
    FamilyMismatch { path: String },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported manifest version {found}, only version {supported} is supported")]
    UnsupportedManifestVersion { found: u32, supported: u32 },

    #[error("invalid path in VM {vm}: {path} (must be relative and have no parent components)")]
    InvalidPath {
        vm: String,
        path: std::path::PathBuf,
    },

    #[error(
        "integrity mismatch for VM {vm} file {path}: size expected {expected_size}, actual {actual_size}, sha256 expected {expected_sha256}, actual {actual_sha256}"
    )]
    IntegrityMismatch {
        vm: String,
        path: std::path::PathBuf,
        expected_size: u64,
        actual_size: u64,
        expected_sha256: String,
        actual_sha256: String,
    },

    #[error("io error for VM {vm} file {path}: {source}")]
    Io {
        vm: String,
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

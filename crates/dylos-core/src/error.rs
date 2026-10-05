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

    #[error(
        "boot parameters for node {node:?} exceed the {budget} bytes budget (actual: {actual})"
    )]
    BootArgsTooLong {
        node: String,
        budget: usize,
        actual: usize,
    },
}

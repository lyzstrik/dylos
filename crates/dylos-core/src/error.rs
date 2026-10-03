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

    #[error("{path}: duplicate IP {ip}")]
    DuplicateIp { path: String, ip: std::net::IpAddr },

    #[error("{path}: segment {segment:?} does not exist")]
    UnknownSegment { path: String, segment: String },

    #[error("{path}: gateway {gateway} is not reachable on any segment")]
    UnreachableGateway {
        path: String,
        gateway: std::net::IpAddr,
    },
}

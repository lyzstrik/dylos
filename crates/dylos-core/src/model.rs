use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LabSpec {
    pub segments: Vec<Segment>,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub name: String,
    pub ipv6: Ipv6Net,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Net>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub name: String,
    pub image: String,
    pub vcpus: u32,
    pub memory: u32,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    #[serde(default)]
    pub static_routes: Vec<StaticRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub name: String,
    pub segment: String,
    pub ipv6: Ipv6Net,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Net>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StaticRoute {
    pub destination: IpNet,
    pub gateway: std::net::IpAddr,
}

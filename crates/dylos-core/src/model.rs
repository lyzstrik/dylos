use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LabSpec {
    /// Enables strict source authentication and rate budgets unless a segment overrides it.
    #[serde(default = "default_peer_filtering")]
    pub peer_filtering: bool,
    pub segments: Vec<Segment>,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    /// Inherits the lab policy when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_filtering: Option<bool>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StaticRoute {
    pub destination: IpNet,
    pub gateway: std::net::IpAddr,
}

impl LabSpec {
    /// Resolves a segment override against the lab policy.
    #[must_use]
    pub fn peer_filtering_for(&self, segment: &Segment) -> bool {
        segment.peer_filtering.unwrap_or(self.peer_filtering)
    }
}

const fn default_peer_filtering() -> bool {
    true
}

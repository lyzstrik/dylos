use ipnet::IpNet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LabSpec {
    pub nodes: Vec<Node>,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
pub struct Segment {
    pub name: String,
    pub cidr: IpNet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Interface {
    pub name: String,
    pub segment: String,
    pub ip: IpNet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StaticRoute {
    pub destination: IpNet,
    pub gateway: std::net::IpAddr,
}

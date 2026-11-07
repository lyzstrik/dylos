use std::collections::BTreeMap;
use std::path::PathBuf;

use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};

use crate::model::{Interface, LabSpec, Node, Segment};

/// Value transition between snapshot A and snapshot B.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValueChange<T> {
    pub from: T,
    pub to: T,
}

/// Generic container for added, removed, and changed items.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffSet<T, C> {
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub added: Vec<T>,
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub removed: Vec<T>,
    #[serde(skip_serializing_if = "Vec::is_empty", default = "Vec::new")]
    pub changed: Vec<C>,
}

impl<T, C> Default for DiffSet<T, C> {
    fn default() -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
        }
    }
}

impl<T, C> DiffSet<T, C> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

pub type SegmentDiff = DiffSet<String, SegmentChange>;
pub type NodeDiff = DiffSet<String, NodeChange>;
pub type InterfaceDiff = DiffSet<NodeInterface, NodeInterfaceChange>;

/// Changes to an individual segment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentChange {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<ValueChange<Ipv6Net>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<ValueChange<Option<Ipv4Net>>>,
}

/// Changes to an individual node's configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeChange {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ValueChange<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<ValueChange<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<ValueChange<u32>>,
}

/// An interface attached to a node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInterface {
    pub node: String,
    pub interface: String,
    pub segment: String,
    pub ipv6: Ipv6Net,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Net>,
}

/// Changes to an individual node interface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInterfaceChange {
    pub node: String,
    pub interface: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment: Option<ValueChange<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<ValueChange<Ipv6Net>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<ValueChange<Option<Ipv4Net>>>,
}

/// Topology changes between two `LabSpec` definitions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopologyDiff {
    #[serde(default)]
    pub segments: SegmentDiff,
    #[serde(default)]
    pub nodes: NodeDiff,
    #[serde(default)]
    pub interfaces: InterfaceDiff,
}

impl TopologyDiff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.nodes.is_empty() && self.interfaces.is_empty()
    }

    /// Computes topology differences between two `LabSpec` specifications.
    #[must_use]
    pub fn diff(spec_a: &LabSpec, spec_b: &LabSpec) -> Self {
        Self {
            segments: diff_segments(spec_a, spec_b),
            nodes: diff_nodes(spec_a, spec_b),
            interfaces: diff_interfaces(spec_a, spec_b),
        }
    }
}

fn diff_segments(spec_a: &LabSpec, spec_b: &LabSpec) -> SegmentDiff {
    let segs_a: BTreeMap<&str, &Segment> = spec_a
        .segments
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();
    let segs_b: BTreeMap<&str, &Segment> = spec_b
        .segments
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();

    SegmentDiff {
        added: segs_b
            .keys()
            .filter(|k| !segs_a.contains_key(*k))
            .map(|k| (*k).to_string())
            .collect(),
        removed: segs_a
            .keys()
            .filter(|k| !segs_b.contains_key(*k))
            .map(|k| (*k).to_string())
            .collect(),
        changed: segs_a
            .iter()
            .filter_map(|(name, sa)| {
                let sb = segs_b.get(name)?;
                let ipv6 = (sa.ipv6 != sb.ipv6).then_some(ValueChange {
                    from: sa.ipv6,
                    to: sb.ipv6,
                });
                let ipv4 = (sa.ipv4 != sb.ipv4).then_some(ValueChange {
                    from: sa.ipv4,
                    to: sb.ipv4,
                });
                (ipv6.is_some() || ipv4.is_some()).then(|| SegmentChange {
                    name: (*name).to_string(),
                    ipv6,
                    ipv4,
                })
            })
            .collect(),
    }
}

fn diff_nodes(spec_a: &LabSpec, spec_b: &LabSpec) -> NodeDiff {
    let nodes_a: BTreeMap<&str, &Node> =
        spec_a.nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    let nodes_b: BTreeMap<&str, &Node> =
        spec_b.nodes.iter().map(|n| (n.name.as_str(), n)).collect();

    NodeDiff {
        added: nodes_b
            .keys()
            .filter(|k| !nodes_a.contains_key(*k))
            .map(|k| (*k).to_string())
            .collect(),
        removed: nodes_a
            .keys()
            .filter(|k| !nodes_b.contains_key(*k))
            .map(|k| (*k).to_string())
            .collect(),
        changed: nodes_a
            .iter()
            .filter_map(|(name, na)| {
                let nb = nodes_b.get(name)?;
                let image = (na.image != nb.image).then(|| ValueChange {
                    from: na.image.clone(),
                    to: nb.image.clone(),
                });
                let vcpus = (na.vcpus != nb.vcpus).then_some(ValueChange {
                    from: na.vcpus,
                    to: nb.vcpus,
                });
                let memory = (na.memory != nb.memory).then_some(ValueChange {
                    from: na.memory,
                    to: nb.memory,
                });
                (image.is_some() || vcpus.is_some() || memory.is_some()).then(|| NodeChange {
                    name: (*name).to_string(),
                    image,
                    vcpus,
                    memory,
                })
            })
            .collect(),
    }
}

fn diff_interfaces(spec_a: &LabSpec, spec_b: &LabSpec) -> InterfaceDiff {
    let ifaces_a: BTreeMap<(&str, &str), &Interface> = spec_a
        .nodes
        .iter()
        .flat_map(|n| {
            n.interfaces
                .iter()
                .map(|i| ((n.name.as_str(), i.name.as_str()), i))
        })
        .collect();
    let ifaces_b: BTreeMap<(&str, &str), &Interface> = spec_b
        .nodes
        .iter()
        .flat_map(|n| {
            n.interfaces
                .iter()
                .map(|i| ((n.name.as_str(), i.name.as_str()), i))
        })
        .collect();

    InterfaceDiff {
        added: ifaces_b
            .iter()
            .filter(|(k, _)| !ifaces_a.contains_key(*k))
            .map(|(&(node, iface), ib)| NodeInterface {
                node: node.to_string(),
                interface: iface.to_string(),
                segment: ib.segment.clone(),
                ipv6: ib.ipv6,
                ipv4: ib.ipv4,
            })
            .collect(),
        removed: ifaces_a
            .iter()
            .filter(|(k, _)| !ifaces_b.contains_key(*k))
            .map(|(&(node, iface), ia)| NodeInterface {
                node: node.to_string(),
                interface: iface.to_string(),
                segment: ia.segment.clone(),
                ipv6: ia.ipv6,
                ipv4: ia.ipv4,
            })
            .collect(),
        changed: ifaces_a
            .iter()
            .filter_map(|(&(node, iface), ia)| {
                let ib = ifaces_b.get(&(node, iface))?;
                let segment = (ia.segment != ib.segment).then(|| ValueChange {
                    from: ia.segment.clone(),
                    to: ib.segment.clone(),
                });
                let ipv6 = (ia.ipv6 != ib.ipv6).then_some(ValueChange {
                    from: ia.ipv6,
                    to: ib.ipv6,
                });
                let ipv4 = (ia.ipv4 != ib.ipv4).then_some(ValueChange {
                    from: ia.ipv4,
                    to: ib.ipv4,
                });
                (segment.is_some() || ipv6.is_some() || ipv4.is_some()).then(|| {
                    NodeInterfaceChange {
                        node: node.to_string(),
                        interface: iface.to_string(),
                        segment,
                        ipv6,
                        ipv4,
                    }
                })
            })
            .collect(),
    }
}

/// Comparison result for a single snapshot file (state or memory).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiff {
    pub identical: bool,
    pub size_a: u64,
    pub size_b: u64,
}

/// Comparison result for an individual VM disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskDiff {
    pub path: PathBuf,
    pub identical: bool,
    pub size_a: u64,
    pub size_b: u64,
}

/// Comparison result for all files of a single VM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmDiff {
    pub state: FileDiff,
    pub memory: FileDiff,
    pub disks: Vec<DiskDiff>,
}

/// Complete diff between two lab snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDiff {
    pub topology: TopologyDiff,
    pub vms: BTreeMap<String, VmDiff>,
}

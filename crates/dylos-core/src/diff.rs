use std::collections::BTreeMap;
use std::path::PathBuf;

use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};

use crate::model::{Interface, LabSpec, Node, Segment, StaticRoute};

/// Status of a file, disk, or VM comparison across two snapshots.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    Identical,
    Changed,
    Added,
    Removed,
}

/// Value transition between snapshot A and snapshot B.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValueChange<T> {
    pub from: T,
    pub to: T,
}

/// Generic container for added, removed, and changed items.
///
/// Keys are stably emitted in serialized output (always serializing arrays even if empty).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffSet<T, C> {
    pub added: Vec<T>,
    pub removed: Vec<T>,
    pub changed: Vec<C>,
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

/// Changes to an individual segment. Unchanged fields serialize as explicit null.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SegmentChange {
    pub name: String,
    pub ipv6: Option<ValueChange<Ipv6Net>>,
    pub ipv4: Option<ValueChange<Option<Ipv4Net>>>,
}

/// Changes to an individual node's configuration. Unchanged fields serialize as explicit null.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeChange {
    pub name: String,
    pub image: Option<ValueChange<String>>,
    pub vcpus: Option<ValueChange<u32>>,
    pub memory: Option<ValueChange<u32>>,
    pub static_routes: Option<ValueChange<Vec<StaticRoute>>>,
}

/// An interface attached to a node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInterface {
    pub node: String,
    pub interface: String,
    pub segment: String,
    pub ipv6: Ipv6Net,
    pub ipv4: Option<Ipv4Net>,
}

/// Changes to an individual node interface. Unchanged fields serialize as explicit null.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInterfaceChange {
    pub node: String,
    pub interface: String,
    pub segment: Option<ValueChange<String>>,
    pub ipv6: Option<ValueChange<Ipv6Net>>,
    pub ipv4: Option<ValueChange<Option<Ipv4Net>>>,
}

/// Topology changes between two `LabSpec` definitions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopologyDiff {
    pub segments: SegmentDiff,
    pub nodes: NodeDiff,
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

fn compute_diff_set<K: Ord, V, T, C>(
    map_a: &BTreeMap<K, V>,
    map_b: &BTreeMap<K, V>,
    mut make_item: impl FnMut(&K, &V) -> T,
    mut diff_item: impl FnMut(&K, &V, &V) -> Option<C>,
) -> DiffSet<T, C> {
    let added = map_b
        .iter()
        .filter(|(k, _)| !map_a.contains_key(*k))
        .map(|(k, v)| make_item(k, v))
        .collect();
    let removed = map_a
        .iter()
        .filter(|(k, _)| !map_b.contains_key(*k))
        .map(|(k, v)| make_item(k, v))
        .collect();
    let changed = map_a
        .iter()
        .filter_map(|(k, va)| {
            let vb = map_b.get(k)?;
            diff_item(k, va, vb)
        })
        .collect();
    DiffSet {
        added,
        removed,
        changed,
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

    compute_diff_set(
        &segs_a,
        &segs_b,
        |k, _| (*k).to_string(),
        |name, sa, sb| {
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
        },
    )
}

fn diff_nodes(spec_a: &LabSpec, spec_b: &LabSpec) -> NodeDiff {
    let nodes_a: BTreeMap<&str, &Node> =
        spec_a.nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    let nodes_b: BTreeMap<&str, &Node> =
        spec_b.nodes.iter().map(|n| (n.name.as_str(), n)).collect();

    compute_diff_set(
        &nodes_a,
        &nodes_b,
        |k, _| (*k).to_string(),
        |name, na, nb| {
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
            let static_routes = (na.static_routes != nb.static_routes).then(|| ValueChange {
                from: na.static_routes.clone(),
                to: nb.static_routes.clone(),
            });
            (image.is_some() || vcpus.is_some() || memory.is_some() || static_routes.is_some())
                .then(|| NodeChange {
                    name: (*name).to_string(),
                    image,
                    vcpus,
                    memory,
                    static_routes,
                })
        },
    )
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

    compute_diff_set(
        &ifaces_a,
        &ifaces_b,
        |&(node, iface), ib| NodeInterface {
            node: node.to_string(),
            interface: iface.to_string(),
            segment: ib.segment.clone(),
            ipv6: ib.ipv6,
            ipv4: ib.ipv4,
        },
        |&(node, iface), ia, ib| {
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
            (segment.is_some() || ipv6.is_some() || ipv4.is_some()).then(|| NodeInterfaceChange {
                node: node.to_string(),
                interface: iface.to_string(),
                segment,
                ipv6,
                ipv4,
            })
        },
    )
}

/// Comparison result for a single snapshot file (state or memory).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiff {
    pub status: DiffStatus,
    pub size_a: Option<u64>,
    pub size_b: Option<u64>,
}

impl FileDiff {
    #[must_use]
    pub fn is_identical(&self) -> bool {
        self.status == DiffStatus::Identical
    }
}

/// Comparison result for an individual VM disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskDiff {
    pub path: PathBuf,
    pub status: DiffStatus,
    pub size_a: Option<u64>,
    pub size_b: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<DiskContentDiff>,
}

/// Fixed 4096-byte block comparison; changed bytes include the entire differing block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskContentDiff {
    pub changed_blocks: u64,
    pub changed_bytes: u64,
}

impl DiskDiff {
    #[must_use]
    pub fn is_identical(&self) -> bool {
        self.status == DiffStatus::Identical
    }
}

/// Comparison result for all files of a single VM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmDiff {
    pub status: DiffStatus,
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

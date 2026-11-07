use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotNode {
    pub id: String,
    pub lab_id: String,
    pub created_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_lab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SnapshotNode>,
}

fn assemble(idx: usize, nodes: &[SnapshotNode], children: &[Vec<usize>]) -> SnapshotNode {
    let mut node = nodes[idx].clone();
    for &child_idx in &children[idx] {
        node.children.push(assemble(child_idx, nodes, children));
    }
    node
}

/// Builds a deterministic lineage tree (forest of roots) from a collection of snapshot nodes.
///
/// If a snapshot references a parent snapshot that does not exist in the collection (e.g. deleted),
/// a missing parent node is generated as a root to preserve the ancestry context.
#[must_use]
pub fn build_lineage_tree(mut nodes: Vec<SnapshotNode>) -> Vec<SnapshotNode> {
    nodes.sort_by(|a, b| {
        a.created_at_unix_ms
            .cmp(&b.created_at_unix_ms)
            .then_with(|| a.lab_id.cmp(&b.lab_id))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut by_scoped: HashMap<String, usize> = HashMap::new();

    for (i, node) in nodes.iter().enumerate() {
        by_id.insert(node.id.clone(), i);
        by_scoped.insert(format!("{}/{}", node.lab_id, node.id), i);
    }

    let mut parent_pointers: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut missing_parents: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, node) in nodes.iter().enumerate() {
        if let Some(parent_ref) = &node.parent_snapshot_id {
            let matched = by_scoped.get(parent_ref).copied().or_else(|| {
                if let Some(parent_lab) = &node.parent_lab_id {
                    let qualified = format!("{parent_lab}/{parent_ref}");
                    by_scoped.get(&qualified).copied()
                } else {
                    None
                }
                .or_else(|| by_id.get(parent_ref).copied())
            });

            if let Some(p_idx) = matched {
                if p_idx != i {
                    parent_pointers[i] = Some(p_idx);
                }
            } else {
                missing_parents
                    .entry(parent_ref.clone())
                    .or_default()
                    .push(i);
            }
        }
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();

    for (i, parent) in parent_pointers.iter().enumerate() {
        if let Some(p_idx) = *parent {
            children[p_idx].push(i);
        } else if !missing_parents.values().any(|v| v.contains(&i)) {
            roots.push(i);
        }
    }

    let mut result = Vec::new();
    for &r_idx in &roots {
        result.push(assemble(r_idx, &nodes, &children));
    }

    for (missing_id, child_indices) in missing_parents {
        let mut missing_node = SnapshotNode {
            id: missing_id,
            lab_id: String::new(),
            created_at_unix_ms: 0,
            parent_lab_id: None,
            parent_snapshot_id: None,
            missing: true,
            children: Vec::new(),
        };
        for child_idx in child_indices {
            missing_node
                .children
                .push(assemble(child_idx, &nodes, &children));
        }
        result.push(missing_node);
    }

    result
}

/// Formats a list of root snapshot nodes into a human-readable tree.
#[must_use]
pub fn format_tree(roots: &[SnapshotNode]) -> String {
    let mut out = String::new();
    for (i, root) in roots.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        format_node(root, "", true, true, &mut out);
    }
    out
}

fn format_node(node: &SnapshotNode, prefix: &str, is_last: bool, is_root: bool, out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let marker = if is_root {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };
    out.push_str(prefix);
    out.push_str(marker);
    if node.missing {
        let _ = write!(out, "[missing: {}]", node.id);
    } else {
        let _ = write!(out, "{} (lab: {})", node.id, node.lab_id);
    }
    let child_prefix = if is_root {
        ""
    } else if is_last {
        "    "
    } else {
        "│   "
    };
    let new_prefix = format!("{prefix}{child_prefix}");
    for (i, child) in node.children.iter().enumerate() {
        let child_is_last = i + 1 == node.children.len();
        format_node(child, &new_prefix, child_is_last, false, out);
    }
}

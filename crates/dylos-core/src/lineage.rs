//! Informational lineage, independent of storage and restore dependencies.
use std::collections::{BTreeMap, BTreeSet};

use crate::manifest::SnapshotParent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotNode {
    pub id: SnapshotParent,
    pub parent: Option<SnapshotParent>,
}

#[derive(Debug, thiserror::Error)]
pub enum LineageError {
    #[error(transparent)]
    Identifier(#[from] crate::Error),
    #[error("duplicate snapshot identity: {0:?}")]
    Duplicate(SnapshotParent),
    #[error("snapshot lineage contains a cycle")]
    Cycle,
}

/// Prints a deterministic forest, preserving missing parents as placeholders.
///
/// # Errors
/// Rejects malformed identifiers, duplicate identities and cycles (including self-links).
pub fn format_tree(nodes: &[SnapshotNode]) -> Result<String, LineageError> {
    let mut parents = BTreeMap::new();
    for node in nodes {
        node.id.validate()?;
        if let Some(parent) = &node.parent {
            parent.validate()?;
        }
        if parents
            .insert(node.id.clone(), node.parent.clone())
            .is_some()
        {
            return Err(LineageError::Duplicate(node.id.clone()));
        }
    }
    let present: BTreeSet<_> = parents.keys().cloned().collect();
    for parent in nodes.iter().filter_map(|node| node.parent.as_ref()) {
        parents.entry(parent.clone()).or_insert(None);
    }
    let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut roots = Vec::new();
    for (id, parent) in &parents {
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(id);
        } else {
            roots.push(id);
        }
    }
    let mut stack: Vec<_> = roots.into_iter().rev().map(|id| (id, 0)).collect();
    let mut visited = 0;
    let mut out = String::new();
    while let Some((id, depth)) = stack.pop() {
        visited += 1;
        out.push_str(&"  ".repeat(depth));
        out.push_str(&id.lab_id);
        out.push('/');
        out.push_str(&id.snapshot);
        if !present.contains(id) {
            out.push_str(" [missing]");
        }
        out.push('\n');
        if let Some(children) = children.get(id) {
            stack.extend(children.iter().rev().map(|child| (*child, depth + 1)));
        }
    }
    if visited != parents.len() {
        return Err(LineageError::Cycle);
    }
    Ok(out)
}

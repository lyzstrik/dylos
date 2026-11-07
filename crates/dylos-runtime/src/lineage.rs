use std::path::{Path, PathBuf};

use dylos_core::lineage::SnapshotNode;
use dylos_store::SnapshotOpener;

use crate::lab::LabError;
use crate::snapshot::verify_snapshot;

fn find_snapshot_dirs(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 {
        return;
    }
    if root.join("manifest.json").is_file() {
        out.push(root.to_path_buf());
        return;
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type()
                && file_type.is_dir()
            {
                subdirs.push(entry.path());
            }
        }
        subdirs.sort();
        for dir in subdirs {
            find_snapshot_dirs(&dir, max_depth - 1, out);
        }
    }
}

async fn load_and_verify_node(path: &Path) -> Result<SnapshotNode, LabError> {
    let path_buf = path.to_path_buf();
    let (opener, manifest) = tokio::task::spawn_blocking(move || {
        let opener = SnapshotOpener::new(&path_buf).map_err(|e| dylos_core::Error::Io {
            vm: "snapshot".into(),
            path: path_buf.clone(),
            source: e,
        })?;
        let manifest = opener.load_manifest()?;
        Ok::<_, LabError>((opener, manifest))
    })
    .await
    .map_err(|e| LabError::Task {
        lab: "lineage".into(),
        source: e,
    })??;

    verify_snapshot(&opener, &manifest).await?;

    let id = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let lab_id = manifest
        .parent
        .as_ref()
        .map(|p| p.lab_id.clone())
        .or_else(|| {
            path.parent()
                .filter(|p| p.file_name().is_none_or(|n| n != "snapshots"))
                .and_then(Path::file_name)
                .and_then(|s| s.to_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    let (parent_lab_id, parent_snapshot_id) = if let Some(parent) = manifest.parent {
        (Some(parent.lab_id), parent.snapshot)
    } else {
        (None, None)
    };

    Ok(SnapshotNode {
        id,
        lab_id,
        created_at_unix_ms: manifest.created_at_unix_ms,
        parent_lab_id,
        parent_snapshot_id,
        missing: false,
        children: Vec::new(),
    })
}

/// Discovers, verifies and builds the snapshot lineage tree rooted at `root_path`.
///
/// # Errors
/// Returns [`LabError`] if any snapshot fails integrity verification or manifest validation.
pub async fn load_lineage_tree(root_path: &Path) -> Result<Vec<SnapshotNode>, LabError> {
    let scan_root = if root_path.join("manifest.json").is_file()
        && root_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|n| n == "snapshots")
    {
        root_path
            .parent()
            .and_then(Path::parent)
            .unwrap_or(root_path)
    } else {
        root_path
    };

    let mut snap_dirs = Vec::new();
    find_snapshot_dirs(scan_root, 4, &mut snap_dirs);

    let mut nodes = Vec::new();
    for dir in snap_dirs {
        nodes.push(load_and_verify_node(&dir).await?);
    }

    Ok(dylos_core::lineage::build_lineage_tree(nodes))
}

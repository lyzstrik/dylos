//! Read-only discovery of published snapshot lineage.
use std::path::{Path, PathBuf};

use dylos_core::lineage::{SnapshotNode, format_tree};
use dylos_core::manifest::SnapshotParent;
use dylos_store::SnapshotOpener;

/// Reads a snapshot, lab snapshot directory, or the store's snapshots directory.
///
/// # Errors
/// Rejects IO errors, symlinks, invalid manifests, payload corruption and invalid graphs.
pub async fn snapshot_tree(root: &Path) -> Result<String, crate::LabError> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || load_tree(&root))
        .await
        .map_err(|source| crate::LabError::Task {
            lab: "lineage".into(),
            source,
        })?
        .map_err(|source| {
            dylos_core::Error::Io {
                vm: "lineage".into(),
                path: PathBuf::new(),
                source,
            }
            .into()
        })
}

fn load_tree(root: &Path) -> std::io::Result<String> {
    let mut pending = vec![(root.to_owned(), 0)];
    let mut nodes = Vec::new();
    while let Some((path, depth)) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "not a snapshot directory: {}",
                path.display()
            )));
        }
        if path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b".staging-"))
        {
            continue;
        }
        match std::fs::symlink_metadata(path.join("manifest.json")) {
            Ok(_) => {
                let opener = SnapshotOpener::new(&path)?;
                let manifest = opener.load_manifest().map_err(std::io::Error::other)?;
                opener.verify(&manifest).map_err(std::io::Error::other)?;
                let name = |path: &Path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                        .ok_or_else(|| std::io::Error::other("invalid snapshot identity"))
                };
                let id = SnapshotParent {
                    lab_id: name(
                        path.parent()
                            .ok_or_else(|| std::io::Error::other("missing lab identity"))?,
                    )?,
                    snapshot: name(&path)?,
                };
                nodes.push(SnapshotNode {
                    id,
                    parent: manifest.parent,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && depth < 2 => {
                for entry in std::fs::read_dir(&path)? {
                    let entry = entry?;
                    if entry.file_type()?.is_symlink() {
                        return Err(std::io::Error::other(format!(
                            "symlink in snapshot collection: {}",
                            entry.path().display()
                        )));
                    }
                    if entry.file_type()?.is_dir() {
                        pending.push((entry.path(), depth + 1));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    format_tree(&nodes).map_err(std::io::Error::other)
}

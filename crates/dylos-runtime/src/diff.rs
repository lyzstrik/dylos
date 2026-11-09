use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use dylos_core::diff::{DiffStatus, DiskDiff, FileDiff, SnapshotDiff, TopologyDiff, VmDiff};
use dylos_core::manifest::{FileMeta, SnapshotManifest};
use dylos_store::SnapshotOpener;

use crate::lab::LabError;
use crate::snapshot::verify_snapshot;

/// Single call site for snapshot loading and integrity verification.
async fn load_and_verify_snapshot(path: &Path) -> Result<SnapshotManifest, LabError> {
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
        lab: "diff".into(),
        source: e,
    })??;

    verify_snapshot(&opener, &manifest).await?;
    Ok(manifest)
}

// Compare only the digest that verification checked. Mixed sealing modes have
// incomparable digests and conservatively report a change.
fn file_diff(a: Option<&FileMeta>, b: Option<&FileMeta>) -> FileDiff {
    let status = match (a, b) {
        (Some(a), Some(b))
            if a.size == b.size
                && match (&a.fsverity, &b.fsverity) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => a.sha256 == b.sha256,
                    _ => false,
                } =>
        {
            DiffStatus::Identical
        }
        (Some(_), Some(_)) => DiffStatus::Changed,
        (Some(_), None) => DiffStatus::Removed,
        (None, Some(_)) => DiffStatus::Added,
        (None, None) => DiffStatus::Identical,
    };
    FileDiff {
        status,
        size_a: a.map(|f| f.size),
        size_b: b.map(|f| f.size),
    }
}

/// Compares two snapshots of a lab, reporting topology changes and per-VM file differences.
///
/// # Errors
/// Returns [`LabError`] if opening or verifying either snapshot fails.
pub async fn diff_snapshots(path_a: &Path, path_b: &Path) -> Result<SnapshotDiff, LabError> {
    let manifest_a = load_and_verify_snapshot(path_a).await?;
    let manifest_b = load_and_verify_snapshot(path_b).await?;

    let topology = TopologyDiff::diff(&manifest_a.lab_spec, &manifest_b.lab_spec);

    let mut vm_names: BTreeSet<&str> = manifest_a.vms.keys().map(String::as_str).collect();
    vm_names.extend(manifest_b.vms.keys().map(String::as_str));

    let mut vms = BTreeMap::new();
    for vm_name in vm_names {
        let a = manifest_a.vms.get(vm_name);
        let b = manifest_b.vms.get(vm_name);
        let state = file_diff(a.map(|v| &v.state_file), b.map(|v| &v.state_file));
        let memory = file_diff(a.map(|v| &v.memory_file), b.map(|v| &v.memory_file));
        let disks_a: BTreeMap<_, _> = a
            .into_iter()
            .flat_map(|v| &v.disks)
            .map(|d| (&d.path, d))
            .collect();
        let disks_b: BTreeMap<_, _> = b
            .into_iter()
            .flat_map(|v| &v.disks)
            .map(|d| (&d.path, d))
            .collect();
        let paths: BTreeSet<_> = disks_a.keys().chain(disks_b.keys()).copied().collect();
        let disks: Vec<_> = paths
            .into_iter()
            .map(|path| {
                let file = file_diff(disks_a.get(path).copied(), disks_b.get(path).copied());
                DiskDiff {
                    path: path.clone(),
                    status: file.status,
                    size_a: file.size_a,
                    size_b: file.size_b,
                }
            })
            .collect();
        let status = match (a, b) {
            (None, Some(_)) => DiffStatus::Added,
            (Some(_), None) => DiffStatus::Removed,
            _ if state.is_identical()
                && memory.is_identical()
                && disks.iter().all(DiskDiff::is_identical) =>
            {
                DiffStatus::Identical
            }
            _ => DiffStatus::Changed,
        };
        let vm_diff = VmDiff {
            status,
            state,
            memory,
            disks,
        };
        vms.insert(vm_name.to_string(), vm_diff);
    }

    Ok(SnapshotDiff { topology, vms })
}

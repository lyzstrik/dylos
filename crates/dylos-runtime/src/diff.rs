use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use dylos_core::diff::{DiffStatus, DiskDiff, FileDiff, SnapshotDiff, TopologyDiff, VmDiff};
use dylos_core::manifest::{FileMeta, SnapshotManifest, VmManifest};
use dylos_store::SnapshotOpener;

use crate::lab::LabError;
use crate::snapshot::verify_snapshot;

/// Single call site for snapshot loading and integrity verification.
async fn load_and_verify_snapshot(
    path: &Path,
) -> Result<(SnapshotOpener, SnapshotManifest), LabError> {
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
    Ok((opener, manifest))
}

fn solo_vm_diff(vm: &VmManifest, status: DiffStatus) -> VmDiff {
    let (state_a, state_b) = match status {
        DiffStatus::Added => (None, Some(vm.state_file.size)),
        DiffStatus::Removed => (Some(vm.state_file.size), None),
        _ => (None, None),
    };
    let (mem_a, mem_b) = match status {
        DiffStatus::Added => (None, Some(vm.memory_file.size)),
        DiffStatus::Removed => (Some(vm.memory_file.size), None),
        _ => (None, None),
    };
    let mut disks: Vec<DiskDiff> = vm
        .disks
        .iter()
        .map(|d| DiskDiff {
            path: d.path.clone(),
            status,
            size_a: if status == DiffStatus::Removed {
                Some(d.size)
            } else {
                None
            },
            size_b: if status == DiffStatus::Added {
                Some(d.size)
            } else {
                None
            },
        })
        .collect();
    disks.sort_by(|d1, d2| d1.path.cmp(&d2.path));
    VmDiff {
        status,
        state: FileDiff {
            status,
            size_a: state_a,
            size_b: state_b,
        },
        memory: FileDiff {
            status,
            size_a: mem_a,
            size_b: mem_b,
        },
        disks,
    }
}

/// Compares two snapshots of a lab, reporting topology changes and per-VM file differences.
///
/// # Errors
/// Returns [`LabError`] if opening or verifying either snapshot fails.
pub async fn diff_snapshots(path_a: &Path, path_b: &Path) -> Result<SnapshotDiff, LabError> {
    let (_opener_a, manifest_a) = load_and_verify_snapshot(path_a).await?;
    let (_opener_b, manifest_b) = load_and_verify_snapshot(path_b).await?;

    let topology = TopologyDiff::diff(&manifest_a.lab_spec, &manifest_b.lab_spec);

    let mut vm_names: BTreeSet<&str> = manifest_a.vms.keys().map(String::as_str).collect();
    vm_names.extend(manifest_b.vms.keys().map(String::as_str));

    let mut vms = BTreeMap::new();
    for vm_name in vm_names {
        let vm_diff = match (manifest_a.vms.get(vm_name), manifest_b.vms.get(vm_name)) {
            (Some(a), Some(b)) => {
                let state_status = if a.state_file.sha256 == b.state_file.sha256 {
                    DiffStatus::Identical
                } else {
                    DiffStatus::Changed
                };
                let state = FileDiff {
                    status: state_status,
                    size_a: Some(a.state_file.size),
                    size_b: Some(b.state_file.size),
                };

                let mem_status = if a.memory_file.sha256 == b.memory_file.sha256 {
                    DiffStatus::Identical
                } else {
                    DiffStatus::Changed
                };
                let memory = FileDiff {
                    status: mem_status,
                    size_a: Some(a.memory_file.size),
                    size_b: Some(b.memory_file.size),
                };

                let mut disks_b: BTreeMap<&Path, &FileMeta> =
                    b.disks.iter().map(|d| (d.path.as_path(), d)).collect();
                let mut disks = Vec::new();

                for disk_a in &a.disks {
                    if let Some(disk_b) = disks_b.remove(disk_a.path.as_path()) {
                        let disk_status = if disk_a.sha256 == disk_b.sha256 {
                            DiffStatus::Identical
                        } else {
                            DiffStatus::Changed
                        };
                        disks.push(DiskDiff {
                            path: disk_a.path.clone(),
                            status: disk_status,
                            size_a: Some(disk_a.size),
                            size_b: Some(disk_b.size),
                        });
                    } else {
                        disks.push(DiskDiff {
                            path: disk_a.path.clone(),
                            status: DiffStatus::Removed,
                            size_a: Some(disk_a.size),
                            size_b: None,
                        });
                    }
                }
                for disk_b in disks_b.values() {
                    disks.push(DiskDiff {
                        path: disk_b.path.clone(),
                        status: DiffStatus::Added,
                        size_a: None,
                        size_b: Some(disk_b.size),
                    });
                }
                disks.sort_by(|d1, d2| d1.path.cmp(&d2.path));

                let vm_status = if state.status == DiffStatus::Identical
                    && memory.status == DiffStatus::Identical
                    && disks.iter().all(|d| d.status == DiffStatus::Identical)
                {
                    DiffStatus::Identical
                } else {
                    DiffStatus::Changed
                };

                VmDiff {
                    status: vm_status,
                    state,
                    memory,
                    disks,
                }
            }
            (Some(a), None) => solo_vm_diff(a, DiffStatus::Removed),
            (None, Some(b)) => solo_vm_diff(b, DiffStatus::Added),
            (None, None) => continue,
        };
        vms.insert(vm_name.to_string(), vm_diff);
    }

    Ok(SnapshotDiff { topology, vms })
}

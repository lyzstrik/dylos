use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use dylos_core::diff::{DiskDiff, FileDiff, SnapshotDiff, TopologyDiff, VmDiff};
use dylos_core::manifest::{FileMeta, SnapshotManifest, VmManifest};
use dylos_store::SnapshotOpener;

use crate::lab::LabError;
use crate::snapshot::verify_snapshot;

/// Single call site for snapshot loading and integrity verification.
async fn load_and_verify_snapshot(
    path: &Path,
) -> Result<(SnapshotOpener, SnapshotManifest), LabError> {
    let opener = SnapshotOpener::new(path).map_err(|e| dylos_core::Error::Io {
        vm: "snapshot".into(),
        path: path.to_path_buf(),
        source: e,
    })?;
    let manifest_file =
        opener
            .open(Path::new("manifest.json"))
            .map_err(|e| dylos_core::Error::Io {
                vm: "snapshot".into(),
                path: PathBuf::from("manifest.json"),
                source: e,
            })?;
    let mut json = String::new();
    manifest_file
        .take(1024 * 1024 + 1)
        .read_to_string(&mut json)
        .map_err(|e| dylos_core::Error::Io {
            vm: "snapshot".into(),
            path: PathBuf::from("manifest.json"),
            source: e,
        })?;
    if json.len() > 1024 * 1024 {
        return Err(dylos_core::Error::Io {
            vm: "snapshot".into(),
            path: PathBuf::from("manifest.json"),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "manifest.json exceeds 1 MiB",
            ),
        }
        .into());
    }
    let manifest = SnapshotManifest::from_json_str(&json)?;
    manifest.lab_spec.validate()?;
    verify_snapshot(&opener, &manifest).await?;
    Ok((opener, manifest))
}

fn solo_vm_diff(vm: &VmManifest, is_a: bool) -> VmDiff {
    let mut disks: Vec<DiskDiff> = vm
        .disks
        .iter()
        .map(|d| DiskDiff {
            path: d.path.clone(),
            identical: false,
            size_a: if is_a { d.size } else { 0 },
            size_b: if is_a { 0 } else { d.size },
        })
        .collect();
    disks.sort_by(|d1, d2| d1.path.cmp(&d2.path));
    VmDiff {
        state: FileDiff {
            identical: false,
            size_a: if is_a { vm.state_file.size } else { 0 },
            size_b: if is_a { 0 } else { vm.state_file.size },
        },
        memory: FileDiff {
            identical: false,
            size_a: if is_a { vm.memory_file.size } else { 0 },
            size_b: if is_a { 0 } else { vm.memory_file.size },
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
                let state = FileDiff {
                    identical: a.state_file.sha256 == b.state_file.sha256,
                    size_a: a.state_file.size,
                    size_b: b.state_file.size,
                };
                let memory = FileDiff {
                    identical: a.memory_file.sha256 == b.memory_file.sha256,
                    size_a: a.memory_file.size,
                    size_b: b.memory_file.size,
                };

                let mut disks_b: BTreeMap<&Path, &FileMeta> =
                    b.disks.iter().map(|d| (d.path.as_path(), d)).collect();
                let mut disks = Vec::new();

                for disk_a in &a.disks {
                    if let Some(disk_b) = disks_b.remove(disk_a.path.as_path()) {
                        disks.push(DiskDiff {
                            path: disk_a.path.clone(),
                            identical: disk_a.sha256 == disk_b.sha256,
                            size_a: disk_a.size,
                            size_b: disk_b.size,
                        });
                    } else {
                        disks.push(DiskDiff {
                            path: disk_a.path.clone(),
                            identical: false,
                            size_a: disk_a.size,
                            size_b: 0,
                        });
                    }
                }
                for disk_b in disks_b.values() {
                    disks.push(DiskDiff {
                        path: disk_b.path.clone(),
                        identical: false,
                        size_a: 0,
                        size_b: disk_b.size,
                    });
                }
                disks.sort_by(|d1, d2| d1.path.cmp(&d2.path));
                VmDiff {
                    state,
                    memory,
                    disks,
                }
            }
            (Some(a), None) => solo_vm_diff(a, true),
            (None, Some(b)) => solo_vm_diff(b, false),
            (None, None) => continue,
        };
        vms.insert(vm_name.to_string(), vm_diff);
    }

    Ok(SnapshotDiff { topology, vms })
}

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use dylos_core::diff::{
    DiffStatus, DiskContentDiff, DiskDiff, FileDiff, SnapshotDiff, TopologyDiff, VmDiff,
};
use dylos_core::manifest::{FileMeta, SnapshotManifest};
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
    diff_snapshots_with_content(path_a, path_b, false).await
}

/// Compares verified snapshots, optionally streaming changed disk contents on the blocking pool.
///
/// # Errors
/// Returns [`LabError`] if opening, verification, or content reads fail.
pub async fn diff_snapshots_with_content(
    path_a: &Path,
    path_b: &Path,
    content: bool,
) -> Result<SnapshotDiff, LabError> {
    let (opener_a, manifest_a) = load_and_verify_snapshot(path_a).await?;
    let (opener_b, manifest_b) = load_and_verify_snapshot(path_b).await?;
    tokio::task::spawn_blocking(move || {
        compare(&opener_a, &manifest_a, &opener_b, &manifest_b, content)
    })
    .await
    .map_err(|source| LabError::Task {
        lab: "diff".into(),
        source,
    })?
}

fn compare(
    opener_a: &SnapshotOpener,
    manifest_a: &SnapshotManifest,
    opener_b: &SnapshotOpener,
    manifest_b: &SnapshotManifest,
    content: bool,
) -> Result<SnapshotDiff, LabError> {
    let topology = TopologyDiff::diff(&manifest_a.lab_spec, &manifest_b.lab_spec);

    let mut vm_names: BTreeSet<&str> = manifest_a.vms.keys().map(String::as_str).collect();
    vm_names.extend(manifest_b.vms.keys().map(String::as_str));

    let mut vms = BTreeMap::new();
    for vm_name in vm_names {
        let a = manifest_a.vms.get(vm_name);
        let b = manifest_b.vms.get(vm_name);
        let state = file_diff(a.map(|v| &v.state_file), b.map(|v| &v.state_file));
        let memory = file_diff(a.map(|v| &v.memory_file), b.map(|v| &v.memory_file));
        let disks_a = disk_map(a.map(|v| v.disks.as_slice()).unwrap_or_default(), vm_name)?;
        let disks_b = disk_map(b.map(|v| v.disks.as_slice()).unwrap_or_default(), vm_name)?;
        let paths: BTreeSet<_> = disks_a.keys().chain(disks_b.keys()).collect();
        let mut disks = Vec::new();
        for path in paths {
            let file = file_diff(disks_a.get(path).copied(), disks_b.get(path).copied());
            let counts = if content && file.status != DiffStatus::Identical {
                Some(count_blocks(
                    opener_a,
                    disks_a.get(path).copied(),
                    opener_b,
                    disks_b.get(path).copied(),
                    vm_name,
                    path,
                )?)
            } else {
                None
            };
            disks.push(DiskDiff {
                path: path.clone(),
                status: file.status,
                size_a: file.size_a,
                size_b: file.size_b,
                content: counts,
            });
        }
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

fn disk_map<'a>(
    disks: &'a [FileMeta],
    vm: &str,
) -> Result<BTreeMap<PathBuf, &'a FileMeta>, LabError> {
    let mut map = BTreeMap::new();
    for disk in disks {
        let path: PathBuf = disk
            .path
            .components()
            .filter(|c| *c != std::path::Component::CurDir)
            .collect();
        if map.insert(path.clone(), disk).is_some() {
            return Err(dylos_core::Error::Io {
                vm: vm.into(),
                path,
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, "duplicate disk path"),
            }
            .into());
        }
    }
    Ok(map)
}

fn count_blocks(
    a: &SnapshotOpener,
    meta_a: Option<&FileMeta>,
    b: &SnapshotOpener,
    meta_b: Option<&FileMeta>,
    vm: &str,
    path: &Path,
) -> Result<DiskContentDiff, LabError> {
    let count = || -> std::io::Result<DiskContentDiff> {
        let mut a = meta_a.map(|m| a.open(&m.path)).transpose()?;
        let mut b = meta_b.map(|m| b.open(&m.path)).transpose()?;
        let mut result = DiskContentDiff {
            changed_blocks: 0,
            changed_bytes: 0,
        };
        let mut buf_a = [0; 4096];
        let mut buf_b = [0; 4096];
        loop {
            // Bounded copy fills a block even after short reads.
            let read =
                |file: &mut Option<std::fs::File>, buf: &mut [u8]| -> std::io::Result<usize> {
                    let mut cursor = std::io::Cursor::new(buf);
                    if let Some(file) = file {
                        std::io::copy(&mut file.take(4096), &mut cursor)?;
                    }
                    usize::try_from(cursor.position()).map_err(std::io::Error::other)
                };
            let len_a = read(&mut a, &mut buf_a)?;
            let len_b = read(&mut b, &mut buf_b)?;
            if len_a == 0 && len_b == 0 {
                break;
            }
            if len_a != len_b || buf_a[..len_a] != buf_b[..len_b] {
                result.changed_blocks += 1;
                result.changed_bytes += len_a.max(len_b) as u64;
            }
        }
        Ok(result)
    };
    count().map_err(|source| {
        dylos_core::Error::Io {
            vm: vm.into(),
            path: path.into(),
            source,
        }
        .into()
    })
}

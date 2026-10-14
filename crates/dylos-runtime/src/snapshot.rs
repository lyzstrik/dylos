use std::collections::BTreeMap;
use std::time::{Instant, SystemTime};

use dylos_core::LabSpec;
use dylos_core::manifest::{SnapshotManifest, StepDurations, VmManifest};
use dylos_fc::snapshot::SnapshotCreateParams;
use dylos_store::Snapshot;
use futures_util::future::join_all;
use tracing::Instrument;

use crate::{Lab, LabError, Vm, pause_all, resume_all};

/// Assembles the consistent lab snapshot sequence per ADR-0001:
/// 1. Freeze fabric
/// 2. Pause all VMs
/// 3. Snapshot each VM and clone disks by reflink
/// 4. Write manifest
/// 5. Resume all VMs
/// 6. Thaw fabric
///
/// On failure at any step, VM resume and thaw are guaranteed and partial snapshot files are deleted.
///
/// # Errors
/// Returns [`LabError`] on failure at any step or rollback.
#[allow(clippy::too_many_lines)]
pub async fn snapshot_lab(lab: &mut Lab, snapshot_id: &str) -> Result<SnapshotManifest, LabError> {
    let lab_id = lab.id().to_string();
    let span = tracing::info_span!("snapshot_lab", lab = %lab_id, snapshot = %snapshot_id);

    async {
        // Step 1: Freeze fabric
        let started_freeze = Instant::now();
        if let Some(net) = lab.network_mut() {
            net.freeze().await?;
        }
        let freeze_ms = u64::try_from(started_freeze.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Step 2: Pause all VMs
        let started_pause = Instant::now();
        let pause_res = pause_all(lab).await;
        let pause_ms = u64::try_from(started_pause.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(pause_err) = pause_res {
            if let Some(net) = lab.network_mut()
                && let Err(thaw_err) = net.thaw().await
            {
                return Err(LabError::Rollback {
                    operation: Box::new(pause_err),
                    cleanup: Box::new(LabError::from(thaw_err)),
                });
            }
            return Err(pause_err);
        }

        // Create snapshot directory layout
        let store = lab.store().cloned().unwrap_or_default();
        let mut snapshot = match store.create_snapshot(&lab_id, snapshot_id) {
            Ok(s) => s,
            Err(e) => {
                let cleanup = resume_and_thaw(lab).await;
                return Err(combine_rollback(LabError::Store(e), cleanup));
            }
        };

        // Step 3: Snapshot each VM & clone disks in parallel
        let started_snapshot = Instant::now();
        let results = join_all(
            lab.vms()
                .iter()
                .map(|vm| snapshot_vm(&lab_id, vm, lab.storage(), &snapshot)),
        )
        .await;
        let snapshot_ms = u64::try_from(started_snapshot.elapsed().as_millis()).unwrap_or(u64::MAX);

        let mut first_error = None;
        let mut vms_manifest = BTreeMap::new();
        for result in results {
            match result {
                Ok((node, manifest)) => {
                    vms_manifest.insert(node, manifest);
                }
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }

        if let Some(err) = first_error {
            let cleanup = resume_and_thaw(lab).await;
            let _ = snapshot.remove();
            return Err(combine_rollback(err, cleanup));
        }

        // Step 4: Write manifest
        let created_at_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(0);

        let mut manifest = SnapshotManifest {
            format_version: 1,
            lab_spec: lab.spec().cloned().unwrap_or_else(|| LabSpec {
                nodes: vec![],
                segments: vec![],
            }),
            vms: vms_manifest,
            firecracker_version: "1.17.0".to_string(),
            host_cpu_model: host_cpu_model(),
            created_at_unix_ms,
            step_durations_ms: StepDurations {
                freeze: freeze_ms,
                pause: pause_ms,
                snapshot: snapshot_ms,
                resume: 0,
                thaw: 0,
            },
        };

        if let Err(err) = snapshot.write_manifest(&manifest) {
            let cleanup = resume_and_thaw(lab).await;
            let _ = snapshot.remove();
            return Err(combine_rollback(LabError::Store(err), cleanup));
        }

        // Step 5: Resume all VMs
        let started_resume = Instant::now();
        let resume_res = resume_all(lab).await;
        let resume_ms = u64::try_from(started_resume.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(resume_err) = resume_res {
            let cleanup = if let Some(net) = lab.network_mut() {
                net.thaw().await.err().map(LabError::from)
            } else {
                None
            };
            let _ = snapshot.remove();
            return Err(combine_rollback(resume_err, cleanup));
        }

        // Step 6: Thaw fabric
        let started_thaw = Instant::now();
        let thaw_res = if let Some(net) = lab.network_mut() {
            net.thaw().await.map_err(LabError::from)
        } else {
            Ok(())
        };
        let thaw_ms = u64::try_from(started_thaw.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(thaw_err) = thaw_res {
            let _ = snapshot.remove();
            return Err(thaw_err);
        }

        // Update step durations in manifest and commit
        manifest.step_durations_ms.resume = resume_ms;
        manifest.step_durations_ms.thaw = thaw_ms;
        if let Err(err) = snapshot.write_manifest(&manifest) {
            let _ = snapshot.remove();
            return Err(LabError::Store(err));
        }
        snapshot.commit();

        tracing::info!(
            lab = %lab_id,
            snapshot = %snapshot_id,
            freeze_ms,
            pause_ms,
            snapshot_ms,
            resume_ms,
            thaw_ms,
            total_ms = freeze_ms + pause_ms + snapshot_ms + resume_ms + thaw_ms,
            "lab snapshot completed"
        );

        Ok(manifest)
    }
    .instrument(span)
    .await
}

async fn snapshot_vm(
    lab_id: &str,
    vm: &Vm,
    storage: Option<&dylos_store::Lab>,
    snapshot: &Snapshot,
) -> Result<(String, VmManifest), LabError> {
    let node = vm.node();
    let api = vm.client();

    let params = SnapshotCreateParams::full("/vm.state", "/vm.mem");
    api.create_snapshot(&params)
        .await
        .map_err(|source| LabError::Api {
            lab: lab_id.to_string(),
            vm: node.to_string(),
            source: Box::new(source),
        })?;

    let state_src = vm.paths().root.join("vm.state");
    let state_file = snapshot
        .save_vm_file(&state_src, node, "vm.state")
        .map_err(LabError::from)?;

    let mem_src = vm.paths().root.join("vm.mem");
    let memory_file = snapshot
        .save_vm_file(&mem_src, node, "vm.mem")
        .map_err(LabError::from)?;

    let mut disks = Vec::new();
    if let Some(storage) = storage {
        let vm_paths = storage.vm_paths(node).map_err(LabError::from)?;
        let disk_meta = snapshot
            .clone_disk(&vm_paths.rootfs, node, "rootfs.ext4")
            .map_err(LabError::from)?;
        disks.push(disk_meta);
    }

    Ok((
        node.to_string(),
        VmManifest {
            state_file,
            memory_file,
            disks,
        },
    ))
}

fn host_cpu_model() -> String {
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in info.lines() {
            if let Some((key, val)) = line.split_once(':')
                && key.trim() == "model name"
            {
                return val.trim().to_string();
            }
        }
    }
    std::env::consts::ARCH.to_string()
}

async fn resume_and_thaw(lab: &mut Lab) -> Option<LabError> {
    let mut cleanup = None;
    if let Err(e) = resume_all(lab).await {
        cleanup = Some(e);
    }
    if let Some(net) = lab.network_mut()
        && let Err(e) = net.thaw().await
    {
        let e = LabError::from(e);
        cleanup = Some(match cleanup {
            Some(c) => LabError::Rollback {
                operation: Box::new(c),
                cleanup: Box::new(e),
            },
            None => e,
        });
    }
    cleanup
}

fn combine_rollback(operation: LabError, cleanup: Option<LabError>) -> LabError {
    match cleanup {
        Some(cleanup) => LabError::Rollback {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        },
        None => operation,
    }
}

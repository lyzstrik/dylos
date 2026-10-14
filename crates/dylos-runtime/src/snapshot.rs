use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use dylos_core::LabSpec;
use dylos_core::manifest::{SnapshotManifest, StepDurations, VmManifest};
use dylos_fc::FcClient;
use dylos_fc::snapshot::SnapshotCreateParams;
use dylos_net::FabricThawHandle;
use futures_util::future::join_all;
use tracing::Instrument;

use crate::{Lab, LabError, Vm, pause_all, resume_all};

#[derive(Clone)]
struct VmResumeClient {
    lab: String,
    vm_id: String,
    client: FcClient,
}

impl VmResumeClient {
    async fn resume(&self) -> Result<(), LabError> {
        self.client.resume().await.map_err(|source| LabError::Api {
            lab: self.lab.clone(),
            vm: self.vm_id.clone(),
            source: Box::new(source),
        })
    }
}

pub struct SnapshotLifecycleGuard<'a> {
    lab: &'a mut Lab,
    thaw_handle: Option<FabricThawHandle>,
    resume_clients: Vec<VmResumeClient>,
    frozen: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    disarmed: bool,
}

#[allow(clippy::missing_errors_doc)]
impl<'a> SnapshotLifecycleGuard<'a> {
    pub fn new(lab: &'a mut Lab) -> Self {
        let thaw_handle = lab.network().map(dylos_net::LabNetwork::thaw_handle);
        let resume_clients = lab
            .vms()
            .iter()
            .map(|vm| VmResumeClient {
                lab: lab.id().to_string(),
                vm_id: vm.paths().id.clone(),
                client: vm.client().clone(),
            })
            .collect();

        Self {
            lab,
            thaw_handle,
            resume_clients,
            frozen: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            disarmed: false,
        }
    }

    pub async fn freeze(&mut self) -> Result<(), LabError> {
        if let Some(net) = self.lab.network_mut() {
            net.freeze().await?;
            self.frozen.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<std::time::Duration, LabError> {
        let pause_res = pause_all(self.lab).await;
        match pause_res {
            Ok(skew) => {
                self.paused.store(true, Ordering::SeqCst);
                Ok(skew)
            }
            Err(pause_err) => {
                // pause_all already attempted to resume every VM.
                // Thaw network on rollback.
                if let Err(thaw_err) = self.thaw().await {
                    return Err(LabError::Rollback {
                        operation: Box::new(pause_err),
                        cleanup: Box::new(thaw_err),
                    });
                }
                Err(pause_err)
            }
        }
    }

    pub async fn resume(&mut self) -> Result<(), LabError> {
        if !self.paused.load(Ordering::SeqCst) {
            return Ok(());
        }
        let res = resume_all(self.lab).await;
        match res {
            Ok(()) => {
                self.paused.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(resume_err) => {
                // Attempt retry / recovery of still-paused VMs concurrently
                let results = futures_util::future::join_all(
                    self.resume_clients.iter().map(VmResumeClient::resume),
                )
                .await;
                if results.into_iter().all(|r| r.is_ok()) {
                    self.paused.store(false, Ordering::SeqCst);
                    Ok(())
                } else {
                    // Do NOT thaw! Return error while retaining paused state
                    Err(resume_err)
                }
            }
        }
    }

    pub async fn thaw(&mut self) -> Result<(), LabError> {
        if !self.frozen.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(net) = self.lab.network_mut() {
            net.thaw().await?;
        }
        self.frozen.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub async fn resume_and_thaw(&mut self) -> Option<LabError> {
        let mut cleanup = None;
        if let Err(e) = self.resume().await {
            cleanup = Some(e);
        }
        // Invariant: only thaw if all VMs are confirmed resumed!
        if self.paused.load(Ordering::SeqCst) {
            tracing::error!(
                lab = %self.lab.id(),
                "VMs remained paused after resume failure; fabric remains frozen to preserve invariant"
            );
        } else if let Err(e) = self.thaw().await {
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

    pub fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for SnapshotLifecycleGuard<'_> {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let paused = self.paused.load(Ordering::SeqCst);
        let frozen = self.frozen.load(Ordering::SeqCst);
        if !paused && !frozen {
            return;
        }

        let lab_id = self.lab.id().to_string();
        let resume_clients = self.resume_clients.clone();
        let thaw_handle = self.thaw_handle.clone();

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut resume_ok = true;
                if paused {
                    tracing::warn!(lab = %lab_id, "snapshot cancelled or panicked while paused; recovering VMs");
                    let results = futures_util::future::join_all(
                        resume_clients.iter().map(VmResumeClient::resume)
                    )
                    .await;
                    for (client, res) in resume_clients.iter().zip(results) {
                        if let Err(e) = res {
                            tracing::error!(lab = %lab_id, vm = %client.vm_id, error = %e, "failed to resume VM on cancel");
                            resume_ok = false;
                        }
                    }
                }
                if frozen && resume_ok {
                    tracing::warn!(lab = %lab_id, "snapshot cancelled or panicked while frozen; thawing fabric");
                    if let Some(thaw) = thaw_handle {
                        let res = thaw.thaw().await;
                        if let Err(e) = res {
                            tracing::error!(lab = %lab_id, error = %e, "failed to thaw fabric on cancel");
                        }
                    }
                }
            });
        } else {
            tracing::error!(lab = %lab_id, "snapshot guard dropped outside tokio runtime; cannot background recover");
        }
    }
}

fn clean_jail_snapshot_files(vms: &[Vm]) {
    for vm in vms {
        let root = &vm.paths().root;
        if let Ok(dir) = dylos_store::directory(root, false) {
            for file in ["vm.state", "vm.mem"] {
                let _ = nix::unistd::unlinkat(&dir, file, nix::unistd::UnlinkatFlags::NoRemoveDir);
            }
        }
    }
}

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
    dylos_core::validate_name(snapshot_id, 32)
        .map_err(|e| LabError::Store(dylos_store::Error::InvalidId(e.to_string())))?;
    let span = tracing::info_span!("snapshot_lab", lab = %lab_id, snapshot = %snapshot_id);

    async {
        let mut guard = SnapshotLifecycleGuard::new(lab);

        // Step 1: Freeze fabric
        let started_freeze = Instant::now();
        guard.freeze().await?;
        let freeze_ms = u64::try_from(started_freeze.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Step 2: Pause all VMs
        let started_pause = Instant::now();
        guard.pause().await?;
        let pause_ms = u64::try_from(started_pause.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Create snapshot directory layout
        let store = guard.lab.store().cloned().unwrap_or_default();
        let mut snapshot = match store.create_snapshot(&lab_id, snapshot_id) {
            Ok(s) => s,
            Err(e) => {
                let cleanup = guard.resume_and_thaw().await;
                clean_jail_snapshot_files(guard.lab.vms());
                return Err(combine_rollback(LabError::Store(e), cleanup));
            }
        };

        // Step 3: Snapshot each VM & clone disks in parallel
        let started_snapshot = Instant::now();

        // 3a. Concurrent Firecracker API snapshot requests
        let api_results = join_all(guard.lab.vms().iter().map(|vm| async {
            let node = vm.node();
            let api = vm.client();
            let params = SnapshotCreateParams::full("/vm.state", "/vm.mem");
            api.create_snapshot(&params)
                .await
                .map_err(|source| LabError::Api {
                    lab: lab_id.clone(),
                    vm: node.to_string(),
                    source: Box::new(source),
                })
        }))
        .await;

        if let Some(err) = api_results.into_iter().find_map(std::result::Result::err) {
            let cleanup = guard.resume_and_thaw().await;
            clean_jail_snapshot_files(guard.lab.vms());
            if let Err(e) = snapshot.remove() {
                tracing::error!(%e, "failed to remove partial snapshot");
            }
            return Err(combine_rollback(err, cleanup));
        }

        // 3b. Concurrent owned blocking storage tasks
        let snapshot_arc = Arc::new(snapshot);
        let storage = guard.lab.storage();
        let mut storage_tasks = Vec::new();

        for vm in guard.lab.vms() {
            let snap = Arc::clone(&snapshot_arc);
            let node = vm.node().to_string();
            let state_src = vm.paths().root.join("vm.state");
            let mem_src = vm.paths().root.join("vm.mem");
            let disk_src = storage
                .map(|s| s.vm_paths(&node).map(|p| p.rootfs))
                .transpose()?;
            let task = tokio::task::spawn_blocking(move || {
                let state_file = snap.save_vm_file(&state_src, &node, "vm.state")?;
                let memory_file = snap.save_vm_file(&mem_src, &node, "vm.mem")?;
                let mut disks = Vec::new();
                if let Some(disk_path) = disk_src {
                    let disk_meta = snap.clone_disk(&disk_path, &node, "rootfs.ext4")?;
                    disks.push(disk_meta);
                }
                Ok::<_, LabError>((
                    node,
                    VmManifest {
                        state_file,
                        memory_file,
                        disks,
                    },
                ))
            });
            storage_tasks.push(task);
        }

        let results = join_all(storage_tasks).await;
        let snapshot_ms = u64::try_from(started_snapshot.elapsed().as_millis()).unwrap_or(u64::MAX);

        let mut first_error = None;
        let mut vms_manifest = BTreeMap::new();
        for result in results {
            match result {
                Ok(Ok((node, manifest))) => {
                    vms_manifest.insert(node, manifest);
                }
                Ok(Err(err)) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
                Err(join_err) => {
                    if first_error.is_none() {
                        first_error = Some(LabError::Task {
                            lab: lab_id.clone(),
                            source: join_err,
                        });
                    }
                }
            }
        }

        if let Some(err) = first_error {
            let cleanup = guard.resume_and_thaw().await;
            clean_jail_snapshot_files(guard.lab.vms());
            drop(snapshot_arc);
            return Err(combine_rollback(err, cleanup));
        }

        let Ok(snapshot) = Arc::try_unwrap(snapshot_arc) else {
            let cleanup = guard.resume_and_thaw().await;
            clean_jail_snapshot_files(guard.lab.vms());
            return Err(combine_rollback(
                LabError::Invalid {
                    lab: lab_id.clone(),
                    message: "snapshot handle leaked".into(),
                },
                cleanup,
            ));
        };

        // Step 4: Write initial manifest in spawn_blocking
        let created_at_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(0);

        let mut manifest = SnapshotManifest {
            format_version: 1,
            lab_spec: guard.lab.spec().cloned().unwrap_or_else(|| LabSpec {
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

        let manifest_clone = manifest.clone();
        let write_res = tokio::task::spawn_blocking(move || {
            snapshot.write_manifest(&manifest_clone)?;
            Ok::<_, LabError>(snapshot)
        })
        .await;

        let mut snapshot = match write_res {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => {
                let cleanup = guard.resume_and_thaw().await;
                clean_jail_snapshot_files(guard.lab.vms());
                return Err(combine_rollback(err, cleanup));
            }
            Err(join_err) => {
                let cleanup = guard.resume_and_thaw().await;
                clean_jail_snapshot_files(guard.lab.vms());
                return Err(combine_rollback(
                    LabError::Task {
                        lab: lab_id.clone(),
                        source: join_err,
                    },
                    cleanup,
                ));
            }
        };

        // Step 5: Resume all VMs
        let started_resume = Instant::now();
        let resume_res = guard.resume().await;
        let resume_ms = u64::try_from(started_resume.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(resume_err) = resume_res {
            if let Err(e) = snapshot.remove() {
                tracing::error!(%e, "failed to remove snapshot after resume error");
            }
            clean_jail_snapshot_files(guard.lab.vms());
            return Err(resume_err);
        }

        // Step 6: Thaw fabric
        let started_thaw = Instant::now();
        let thaw_res = guard.thaw().await;
        let thaw_ms = u64::try_from(started_thaw.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(thaw_err) = thaw_res {
            if let Err(e) = snapshot.remove() {
                tracing::error!(%e, "failed to remove snapshot after thaw error");
            }
            clean_jail_snapshot_files(guard.lab.vms());
            return Err(thaw_err);
        }

        // Step 7: Update step durations in manifest and commit atomically
        manifest.step_durations_ms.resume = resume_ms;
        manifest.step_durations_ms.thaw = thaw_ms;
        let final_manifest = manifest.clone();
        tokio::task::spawn_blocking(move || {
            snapshot.write_manifest(&final_manifest)?;
            snapshot.commit();
            Ok::<_, LabError>(())
        })
        .await
        .map_err(|join_err| LabError::Task {
            lab: lab_id.clone(),
            source: join_err,
        })??;

        guard.disarm();

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

fn combine_rollback(operation: LabError, cleanup: Option<LabError>) -> LabError {
    match cleanup {
        Some(cleanup) => LabError::Rollback {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        },
        None => operation,
    }
}

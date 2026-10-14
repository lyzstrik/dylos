use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
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

/// Timings recorded across all snapshot stages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotTimings {
    pub freeze_ms: u64,
    pub pause_ms: u64,
    pub api_snapshot_ms: u64,
    pub storage_ms: u64,
    pub fsync_ms: u64,
    pub reflink_ms: u64,
    pub hash_ms: u64,
    pub manifest_ms: u64,
    pub resume_ms: u64,
    pub thaw_ms: u64,
    pub total_ms: u64,
}

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

struct LifecycleState {
    frozen: AtomicBool,
    paused: AtomicBool,
    api_tasks: Mutex<Vec<tokio::task::JoinHandle<Result<(), LabError>>>>,
    #[allow(clippy::type_complexity)]
    storage_tasks:
        Mutex<Vec<tokio::task::JoinHandle<Result<(String, VmManifest, u64, u64, u64), LabError>>>>,
    partial_snapshot: Mutex<Option<PathBuf>>,
    jail_roots: Mutex<Vec<PathBuf>>,
}

fn poisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct SnapshotLifecycleGuard<'a> {
    lab: &'a mut Lab,
    thaw_handle: Option<FabricThawHandle>,
    resume_clients: Vec<VmResumeClient>,
    state: Arc<LifecycleState>,
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
        let jail_roots = lab.vms().iter().map(|vm| vm.paths().root.clone()).collect();

        Self {
            lab,
            thaw_handle,
            resume_clients,
            state: Arc::new(LifecycleState {
                frozen: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                api_tasks: Mutex::new(Vec::new()),
                storage_tasks: Mutex::new(Vec::new()),
                partial_snapshot: Mutex::new(None),
                jail_roots: Mutex::new(jail_roots),
            }),
            disarmed: false,
        }
    }

    pub fn set_partial_snapshot(&self, path: PathBuf) {
        let mut lock = poisoned(&self.state.partial_snapshot);
        *lock = Some(path);
    }

    pub fn clear_partial_snapshot(&self) {
        let mut lock = poisoned(&self.state.partial_snapshot);
        *lock = None;
    }

    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.state.frozen.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.state.paused.load(Ordering::SeqCst)
    }

    pub async fn freeze(&mut self) -> Result<(), LabError> {
        if let Some(net) = self.lab.network_mut() {
            self.state.frozen.store(true, Ordering::SeqCst);
            if let Err(e) = net.freeze().await {
                if let Err(thaw_err) = net.thaw().await {
                    return Err(LabError::Rollback {
                        operation: Box::new(LabError::Network(e)),
                        cleanup: Box::new(LabError::Network(thaw_err)),
                    });
                }
                self.state.frozen.store(false, Ordering::SeqCst);
                return Err(LabError::Network(e));
            }
        }
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<std::time::Duration, LabError> {
        self.state.paused.store(true, Ordering::SeqCst);
        let pause_res = pause_all(self.lab).await;
        match pause_res {
            Ok(skew) => Ok(skew),
            Err(pause_err) => {
                if matches!(pause_err, LabError::Rollback { .. }) {
                    tracing::error!(
                        lab = %self.lab.id(),
                        "pause failed and resume rollback failed; retaining frozen fabric"
                    );
                    return Err(pause_err);
                }
                self.state.paused.store(false, Ordering::SeqCst);
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
        if !self.state.paused.load(Ordering::SeqCst) {
            return Ok(());
        }
        let res = resume_all(self.lab).await;
        match res {
            Ok(()) => {
                self.state.paused.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(resume_err) => {
                let results = futures_util::future::join_all(
                    self.resume_clients.iter().map(VmResumeClient::resume),
                )
                .await;
                if results.into_iter().all(|r| r.is_ok()) {
                    self.state.paused.store(false, Ordering::SeqCst);
                    Ok(())
                } else {
                    Err(resume_err)
                }
            }
        }
    }

    pub async fn thaw(&mut self) -> Result<(), LabError> {
        if !self.state.frozen.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(net) = self.lab.network_mut() {
            net.thaw().await?;
        }
        self.state.frozen.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub async fn resume_and_thaw(&mut self) -> Option<LabError> {
        let mut cleanup = None;
        if let Err(e) = self.resume().await {
            cleanup = Some(e);
        }
        if self.state.paused.load(Ordering::SeqCst) {
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
        let paused = self.state.paused.load(Ordering::SeqCst);
        let frozen = self.state.frozen.load(Ordering::SeqCst);
        let has_storage = !poisoned(&self.state.storage_tasks).is_empty();
        let has_api = !poisoned(&self.state.api_tasks).is_empty();
        let has_partial = poisoned(&self.state.partial_snapshot).is_some();
        if !paused && !frozen && !has_storage && !has_api && !has_partial {
            return;
        }

        let state = Arc::clone(&self.state);
        let lab_id = self.lab.id().to_string();
        let resume_clients = self.resume_clients.clone();
        let thaw_handle = self.thaw_handle.clone();

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // 1. Wait for in-flight storage tasks before resuming VMs
                let storage_handles = {
                    let mut lock = poisoned(&state.storage_tasks);
                    std::mem::take(&mut *lock)
                };
                for handle in storage_handles {
                    let _ = handle.await;
                }

                // 2. Wait for in-flight API snapshot tasks
                let api_handles = {
                    let mut lock = poisoned(&state.api_tasks);
                    std::mem::take(&mut *lock)
                };
                for handle in api_handles {
                    let _ = handle.await;
                }

                // 3. Recover paused VMs
                let mut resume_ok = true;
                if state.paused.load(Ordering::SeqCst) {
                    tracing::warn!(lab = %lab_id, "snapshot cancelled or panicked while paused; recovering VMs");
                    let results = futures_util::future::join_all(
                        resume_clients.iter().map(VmResumeClient::resume),
                    )
                    .await;
                    for (client, res) in resume_clients.iter().zip(results) {
                        if let Err(e) = res {
                            tracing::error!(lab = %lab_id, vm = %client.vm_id, error = %e, "failed to resume VM on cancel");
                            resume_ok = false;
                        }
                    }
                    if resume_ok {
                        state.paused.store(false, Ordering::SeqCst);
                    }
                }

                // 4. Thaw fabric only if all VMs are confirmed resumed
                if state.frozen.load(Ordering::SeqCst) && resume_ok {
                    tracing::warn!(lab = %lab_id, "snapshot cancelled or panicked while frozen; thawing fabric");
                    if let Some(thaw) = thaw_handle {
                        if let Err(e) = thaw.thaw().await {
                            tracing::error!(lab = %lab_id, error = %e, "failed to thaw fabric on cancel");
                        } else {
                            state.frozen.store(false, Ordering::SeqCst);
                        }
                    }
                }

                // 5. Clean jail snapshot files
                let roots = {
                    let lock = poisoned(&state.jail_roots);
                    lock.clone()
                };
                let _ = tokio::task::spawn_blocking(move || {
                    for root in roots {
                        if let Ok(dir) = dylos_store::directory(&root, false) {
                            for file in ["vm.state", "vm.mem"] {
                                let _ = nix::unistd::unlinkat(&dir, file, nix::unistd::UnlinkatFlags::NoRemoveDir);
                            }
                        }
                    }
                }).await;

                // 6. Clean partial snapshot directory
                let partial = {
                    let mut lock = poisoned(&state.partial_snapshot);
                    lock.take()
                };
                if let Some(path) = partial {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = std::fs::remove_dir_all(&path);
                    }).await;
                }
            });
        } else {
            tracing::error!(lab = %lab_id, "snapshot guard dropped outside tokio runtime; cannot background recover");
        }
    }
}

async fn clean_jail_snapshot_files(vms: &[Vm]) -> Result<(), LabError> {
    let vm_roots: Vec<PathBuf> = vms.iter().map(|vm| vm.paths().root.clone()).collect();
    tokio::task::spawn_blocking(move || {
        let mut first_error = None;
        for root in vm_roots {
            match dylos_store::directory(&root, false) {
                Ok(dir) => {
                    for file in ["vm.state", "vm.mem"] {
                        match nix::unistd::unlinkat(
                            &dir,
                            file,
                            nix::unistd::UnlinkatFlags::NoRemoveDir,
                        ) {
                            Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
                            Err(e) => {
                                if first_error.is_none() {
                                    first_error = Some(LabError::Store(dylos_store::Error::Io {
                                        path: root.join(file),
                                        source: e.into(),
                                    }));
                                }
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(LabError::Store(dylos_store::Error::Io {
                            path: root,
                            source: e,
                        }));
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    })
    .await
    .map_err(|join_err| LabError::Task {
        lab: "jail_cleanup".into(),
        source: join_err,
    })?
}

async fn remove_partial_snapshot(
    snapshot: &mut Option<dylos_store::Snapshot>,
) -> Result<(), LabError> {
    if let Some(mut snap) = snapshot.take() {
        tokio::task::spawn_blocking(move || snap.remove().map_err(LabError::Store))
            .await
            .map_err(|join_err| LabError::Task {
                lab: "snapshot_cleanup".into(),
                source: join_err,
            })??;
    }
    Ok(())
}

fn combine_cleanups(cleanups: Vec<Option<LabError>>) -> Option<LabError> {
    let mut combined: Option<LabError> = None;
    for c in cleanups.into_iter().flatten() {
        combined = Some(match combined {
            Some(prev) => LabError::Rollback {
                operation: Box::new(prev),
                cleanup: Box::new(c),
            },
            None => c,
        });
    }
    combined
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

/// Assembles the consistent lab snapshot sequence per ADR-0001, returning manifest and detailed timings:
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
pub async fn snapshot_lab_with_timings(
    lab: &mut Lab,
    snapshot_id: &str,
) -> Result<(SnapshotManifest, SnapshotTimings), LabError> {
    let lab_id = lab.id().to_string();
    dylos_core::validate_name(snapshot_id, 32)
        .map_err(|e| LabError::Store(dylos_store::Error::InvalidId(e.to_string())))?;
    let span = tracing::info_span!("snapshot_lab", lab = %lab_id, snapshot = %snapshot_id);

    async {
        let started_all = Instant::now();
        let mut guard = SnapshotLifecycleGuard::new(lab);

        // Step 1: Freeze fabric
        let started_freeze = Instant::now();
        guard.freeze().await?;
        let freeze_ms = u64::try_from(started_freeze.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Step 2: Pause all VMs
        let started_pause = Instant::now();
        guard.pause().await?;
        let pause_ms = u64::try_from(started_pause.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Create snapshot directory layout in spawn_blocking
        let store = guard.lab.store().cloned().unwrap_or_default();
        let lab_id_clone = lab_id.clone();
        let snapshot_id_clone = snapshot_id.to_string();
        let create_res = tokio::task::spawn_blocking(move || {
            store
                .create_snapshot(&lab_id_clone, &snapshot_id_clone)
                .map_err(LabError::Store)
        })
        .await
        .map_err(|join_err| LabError::Task {
            lab: lab_id.clone(),
            source: join_err,
        })?;

        let snapshot = match create_res {
            Ok(s) => s,
            Err(e) => {
                let cleanup = combine_cleanups(vec![
                    guard.resume_and_thaw().await,
                    clean_jail_snapshot_files(guard.lab.vms()).await.err(),
                ]);
                return Err(combine_rollback(e, cleanup));
            }
        };

        guard.set_partial_snapshot(snapshot.root().to_path_buf());
        let mut snapshot_opt = Some(snapshot);

        // Step 3: Snapshot each VM & clone disks in parallel
        let started_snapshot = Instant::now();

        // 3a. Concurrent Firecracker API snapshot requests
        let started_api = Instant::now();
        let mut api_handles = Vec::new();
        for vm in guard.lab.vms() {
            let node = vm.node().to_string();
            let api = vm.client().clone();
            let lab_id_for_api = lab_id.clone();
            let handle = tokio::spawn(async move {
                let params = SnapshotCreateParams::full("/vm.state", "/vm.mem");
                api.create_snapshot(&params)
                    .await
                    .map_err(|source| LabError::Api {
                        lab: lab_id_for_api,
                        vm: node,
                        source: Box::new(source),
                    })
            });
            api_handles.push(handle);
        }

        {
            let mut lock = poisoned(&guard.state.api_tasks);
            *lock = api_handles;
        }

        let api_tasks = {
            let mut lock = poisoned(&guard.state.api_tasks);
            std::mem::take(&mut *lock)
        };
        let api_results = join_all(api_tasks).await;
        let api_snapshot_ms = u64::try_from(started_api.elapsed().as_millis()).unwrap_or(u64::MAX);

        let mut first_api_error = None;
        for res in api_results {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_api_error.is_none() {
                        first_api_error = Some(e);
                    }
                }
                Err(join_err) => {
                    if first_api_error.is_none() {
                        first_api_error = Some(LabError::Task {
                            lab: lab_id.clone(),
                            source: join_err,
                        });
                    }
                }
            }
        }

        if let Some(err) = first_api_error {
            let cleanup = combine_cleanups(vec![
                guard.resume_and_thaw().await,
                clean_jail_snapshot_files(guard.lab.vms()).await.err(),
                remove_partial_snapshot(&mut snapshot_opt).await.err(),
            ]);
            return Err(combine_rollback(err, cleanup));
        }

        // 3b. Concurrent owned blocking storage tasks
        let started_storage = Instant::now();
        let snapshot = snapshot_opt.take().ok_or_else(|| LabError::Invalid {
            lab: lab_id.clone(),
            message: "snapshot handle missing".into(),
        })?;
        let snapshot_arc = Arc::new(snapshot);
        let storage = guard.lab.storage();
        let mut storage_handles = Vec::new();

        for vm in guard.lab.vms() {
            let snap = Arc::clone(&snapshot_arc);
            let node = vm.node().to_string();
            let state_src = vm.paths().root.join("vm.state");
            let mem_src = vm.paths().root.join("vm.mem");
            let disk_src = storage
                .map(|s| s.vm_paths(&node).map(|p| p.rootfs))
                .transpose()?;

            let state_max_bytes = 16 * 1024 * 1024;
            let mem_mib = guard
                .lab
                .spec()
                .and_then(|s| s.nodes.iter().find(|n| n.name == node))
                .map_or(256, |n| u64::from(n.memory));
            let mem_max_bytes = (mem_mib * 1024 * 1024) + (16 * 1024 * 1024);

            let handle = tokio::task::spawn_blocking(move || {
                let (state_file, state_hash_ms) =
                    snap.save_vm_file_with_timings(&state_src, &node, "vm.state", state_max_bytes)?;
                let (memory_file, mem_hash_ms) =
                    snap.save_vm_file_with_timings(&mem_src, &node, "vm.mem", mem_max_bytes)?;
                let mut disks = Vec::new();
                let mut fsync_ms = 0u64;
                let mut reflink_ms = 0u64;
                let mut disk_hash_ms = 0u64;
                if let Some(disk_path) = disk_src {
                    let (disk_meta, timings) =
                        snap.clone_disk_with_timings(&disk_path, &node, "rootfs.ext4")?;
                    disks.push(disk_meta);
                    fsync_ms = timings.fsync_ms;
                    reflink_ms = timings.reflink_ms;
                    disk_hash_ms = timings.hash_ms;
                }
                let total_hash_ms = state_hash_ms + mem_hash_ms + disk_hash_ms;
                Ok::<_, LabError>((
                    node,
                    VmManifest {
                        state_file,
                        memory_file,
                        disks,
                    },
                    fsync_ms,
                    reflink_ms,
                    total_hash_ms,
                ))
            });
            storage_handles.push(handle);
        }

        {
            let mut lock = poisoned(&guard.state.storage_tasks);
            *lock = storage_handles;
        }

        let storage_tasks = {
            let mut lock = poisoned(&guard.state.storage_tasks);
            std::mem::take(&mut *lock)
        };
        let results = join_all(storage_tasks).await;
        let storage_ms = u64::try_from(started_storage.elapsed().as_millis()).unwrap_or(u64::MAX);
        let snapshot_ms = u64::try_from(started_snapshot.elapsed().as_millis()).unwrap_or(u64::MAX);

        let mut first_error = None;
        let mut vms_manifest = BTreeMap::new();
        let mut max_fsync_ms = 0u64;
        let mut max_reflink_ms = 0u64;
        let mut max_hash_ms = 0u64;

        for result in results {
            match result {
                Ok(Ok((node, manifest, fsync, reflink, hash))) => {
                    vms_manifest.insert(node, manifest);
                    max_fsync_ms = max_fsync_ms.max(fsync);
                    max_reflink_ms = max_reflink_ms.max(reflink);
                    max_hash_ms = max_hash_ms.max(hash);
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
            drop(snapshot_arc);
            let cleanup = combine_cleanups(vec![
                guard.resume_and_thaw().await,
                clean_jail_snapshot_files(guard.lab.vms()).await.err(),
            ]);
            return Err(combine_rollback(err, cleanup));
        }

        let Ok(snapshot) = Arc::try_unwrap(snapshot_arc) else {
            let cleanup = combine_cleanups(vec![
                guard.resume_and_thaw().await,
                clean_jail_snapshot_files(guard.lab.vms()).await.err(),
            ]);
            return Err(combine_rollback(
                LabError::Invalid {
                    lab: lab_id.clone(),
                    message: "snapshot handle leaked".into(),
                },
                cleanup,
            ));
        };

        snapshot_opt = Some(snapshot);

        // Step 4: Write initial manifest in spawn_blocking
        let started_manifest = Instant::now();
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
        let snap_for_write = snapshot_opt.take().ok_or_else(|| LabError::Invalid {
            lab: lab_id.clone(),
            message: "snapshot handle missing".into(),
        })?;
        let write_res = tokio::task::spawn_blocking(move || {
            snap_for_write.write_manifest(&manifest_clone)?;
            Ok::<_, LabError>(snap_for_write)
        })
        .await;

        let snapshot = match write_res {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => {
                let cleanup = combine_cleanups(vec![
                    guard.resume_and_thaw().await,
                    clean_jail_snapshot_files(guard.lab.vms()).await.err(),
                ]);
                return Err(combine_rollback(err, cleanup));
            }
            Err(join_err) => {
                let cleanup = combine_cleanups(vec![
                    guard.resume_and_thaw().await,
                    clean_jail_snapshot_files(guard.lab.vms()).await.err(),
                ]);
                return Err(combine_rollback(
                    LabError::Task {
                        lab: lab_id.clone(),
                        source: join_err,
                    },
                    cleanup,
                ));
            }
        };

        snapshot_opt = Some(snapshot);
        let manifest_ms = u64::try_from(started_manifest.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Step 5: Resume all VMs
        let started_resume = Instant::now();
        let resume_res = guard.resume().await;
        let resume_ms = u64::try_from(started_resume.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(resume_err) = resume_res {
            let cleanup = combine_cleanups(vec![
                remove_partial_snapshot(&mut snapshot_opt).await.err(),
                clean_jail_snapshot_files(guard.lab.vms()).await.err(),
            ]);
            return Err(combine_rollback(resume_err, cleanup));
        }

        // Step 6: Thaw fabric
        let started_thaw = Instant::now();
        let thaw_res = guard.thaw().await;
        let thaw_ms = u64::try_from(started_thaw.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Err(thaw_err) = thaw_res {
            let cleanup = combine_cleanups(vec![
                remove_partial_snapshot(&mut snapshot_opt).await.err(),
                clean_jail_snapshot_files(guard.lab.vms()).await.err(),
            ]);
            return Err(combine_rollback(thaw_err, cleanup));
        }

        // Step 7: Update step durations in manifest and commit atomically
        manifest.step_durations_ms.resume = resume_ms;
        manifest.step_durations_ms.thaw = thaw_ms;
        let final_manifest = manifest.clone();
        let mut snap_for_commit = snapshot_opt.take().ok_or_else(|| LabError::Invalid {
            lab: lab_id.clone(),
            message: "snapshot handle missing".into(),
        })?;
        let commit_res = tokio::task::spawn_blocking(move || {
            snap_for_commit.write_manifest(&final_manifest)?;
            snap_for_commit.commit();
            Ok::<_, LabError>(())
        })
        .await
        .map_err(|join_err| LabError::Task {
            lab: lab_id.clone(),
            source: join_err,
        })?;

        if let Err(e) = commit_res {
            let cleanup =
                combine_cleanups(vec![clean_jail_snapshot_files(guard.lab.vms()).await.err()]);
            return Err(combine_rollback(e, cleanup));
        }

        guard.clear_partial_snapshot();
        guard.disarm();

        // Clean jail snapshot files on success as well
        let _ = clean_jail_snapshot_files(guard.lab.vms()).await;

        let total_ms = u64::try_from(started_all.elapsed().as_millis()).unwrap_or(u64::MAX);
        let timings = SnapshotTimings {
            freeze_ms,
            pause_ms,
            api_snapshot_ms,
            storage_ms,
            fsync_ms: max_fsync_ms,
            reflink_ms: max_reflink_ms,
            hash_ms: max_hash_ms,
            manifest_ms,
            resume_ms,
            thaw_ms,
            total_ms,
        };

        tracing::info!(
            lab = %lab_id,
            snapshot = %snapshot_id,
            freeze_ms,
            pause_ms,
            snapshot_ms,
            api_snapshot_ms,
            storage_ms,
            fsync_ms = max_fsync_ms,
            reflink_ms = max_reflink_ms,
            hash_ms = max_hash_ms,
            manifest_ms,
            resume_ms,
            thaw_ms,
            total_ms,
            "lab snapshot completed"
        );

        Ok((manifest, timings))
    }
    .instrument(span)
    .await
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
pub async fn snapshot_lab(lab: &mut Lab, snapshot_id: &str) -> Result<SnapshotManifest, LabError> {
    snapshot_lab_with_timings(lab, snapshot_id)
        .await
        .map(|(manifest, _)| manifest)
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

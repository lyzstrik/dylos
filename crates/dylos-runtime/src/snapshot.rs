use std::collections::BTreeMap;
#[cfg(feature = "test-hooks")]
use std::path::Path;
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
use futures_util::FutureExt;
use futures_util::future::join_all;
use tracing::Instrument;

use crate::{Lab, LabError, pause_all, resume_all};

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

#[cfg(feature = "test-hooks")]
type SnapshotHookFn = Box<dyn Fn(&str, &Path) + Send + Sync>;

#[cfg(feature = "test-hooks")]
static SNAPSHOT_HOOK: std::sync::Mutex<Option<SnapshotHookFn>> = std::sync::Mutex::new(None);

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn set_snapshot_test_hook(hook: impl Fn(&str, &Path) + Send + Sync + 'static) {
    if let Ok(mut lock) = SNAPSHOT_HOOK.lock() {
        *lock = Some(Box::new(hook));
    }
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn clear_snapshot_test_hook() {
    if let Ok(mut lock) = SNAPSHOT_HOOK.lock() {
        *lock = None;
    }
}

#[cfg(feature = "test-hooks")]
fn trigger_snapshot_hook(stage: &str, path: &Path) {
    if let Ok(lock) = SNAPSHOT_HOOK.lock()
        && let Some(hook) = lock.as_ref()
    {
        hook(stage, path);
    }
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

/// Owned snapshot parameters passed to the run-to-completion task.
pub struct SnapshotJob {
    pub lab_id: String,
    pub snapshot_id: String,
    pub spec: Option<LabSpec>,
    pub store: dylos_store::Store,
    pub vms: Vec<SnapshotVm>,
    pub fabric: Option<FabricThawHandle>,
}

/// Owned VM descriptor for the snapshot execution task.
#[derive(Clone)]
pub struct SnapshotVm {
    pub node: String,
    pub client: FcClient,
    pub root_dir: PathBuf,
    pub disk_path: Option<PathBuf>,
    pub memory_mib: u64,
}

struct SnapshotExecutionState {
    frozen: AtomicBool,
    paused: AtomicBool,
    partial_snapshot: Mutex<Option<dylos_store::Snapshot>>,
}

fn poisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Compatibility guard for existing callers; the primary snapshot orchestration
/// runs inside the cancellation-independent task in [`snapshot_lab_with_timings`].
pub struct SnapshotLifecycleGuard<'a> {
    lab: &'a mut Lab,
    thaw_handle: Option<FabricThawHandle>,
    resume_clients: Vec<VmResumeClient>,
    frozen: bool,
    paused: bool,
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
            frozen: false,
            paused: false,
            disarmed: false,
        }
    }

    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub async fn freeze(&mut self) -> Result<(), LabError> {
        if let Some(net) = self.lab.network_mut() {
            self.frozen = true;
            if let Err(e) = net.freeze().await {
                self.frozen = false;
                return Err(LabError::Network(e));
            }
        }
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<std::time::Duration, LabError> {
        self.paused = true;
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
                self.paused = false;
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
        if !self.paused {
            return Ok(());
        }
        let res = resume_all(self.lab).await;
        match res {
            Ok(()) => {
                self.paused = false;
                Ok(())
            }
            Err(resume_err) => {
                let results = futures_util::future::join_all(
                    self.resume_clients.iter().map(VmResumeClient::resume),
                )
                .await;
                if results.into_iter().all(|r| r.is_ok()) {
                    self.paused = false;
                    Ok(())
                } else {
                    Err(resume_err)
                }
            }
        }
    }

    pub async fn thaw(&mut self) -> Result<(), LabError> {
        if !self.frozen {
            return Ok(());
        }
        if let Some(net) = self.lab.network_mut() {
            net.thaw().await?;
        }
        self.frozen = false;
        Ok(())
    }

    pub async fn resume_and_thaw(&mut self) -> Option<LabError> {
        let mut cleanup = None;
        if let Err(e) = self.resume().await {
            cleanup = Some(e);
        }
        if self.paused {
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
        if self.disarmed || (!self.paused && !self.frozen) {
            return;
        }

        let paused = self.paused;
        let frozen = self.frozen;
        let lab_id = self.lab.id().to_string();
        let resume_clients = self.resume_clients.clone();
        let thaw_handle = self.thaw_handle.clone();

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut resume_ok = true;
                if paused {
                    tracing::warn!(lab = %lab_id, "snapshot guard dropped while paused; recovering VMs");
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
                }
                if frozen && resume_ok {
                    tracing::warn!(lab = %lab_id, "snapshot guard dropped while frozen; thawing fabric");
                    if let Some(thaw) = thaw_handle
                        && let Err(e) = thaw.thaw().await
                    {
                        tracing::error!(lab = %lab_id, error = %e, "failed to thaw fabric on cancel");
                    }
                }
            });
        }
    }
}

async fn clean_jail_files_for_vms(vms: &[SnapshotVm]) -> Result<(), LabError> {
    let vm_roots: Vec<PathBuf> = vms.iter().map(|vm| vm.root_dir.clone()).collect();
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

async fn run_snapshot_recovery(
    job: &SnapshotJob,
    state: &SnapshotExecutionState,
) -> Option<LabError> {
    let mut cleanups = Vec::new();

    // 1. Recover paused VMs
    let mut resume_ok = true;
    if state.paused.load(Ordering::SeqCst) {
        tracing::warn!(lab = %job.lab_id, "snapshot recovering paused VMs");
        let results = join_all(job.vms.iter().map(|vm| vm.client.resume())).await;
        for (vm, res) in job.vms.iter().zip(results) {
            if let Err(e) = res {
                tracing::error!(
                    lab = %job.lab_id,
                    vm = %vm.node,
                    error = %e,
                    "failed to resume VM on recovery"
                );
                resume_ok = false;
                cleanups.push(Some(LabError::Api {
                    lab: job.lab_id.clone(),
                    vm: vm.node.clone(),
                    source: Box::new(e),
                }));
            }
        }
        if resume_ok {
            state.paused.store(false, Ordering::SeqCst);
        }
    }

    // 2. Thaw fabric only if all VMs are confirmed resumed
    if state.frozen.load(Ordering::SeqCst) {
        if resume_ok {
            if let Some(fabric) = &job.fabric {
                tracing::warn!(lab = %job.lab_id, "snapshot thawing fabric on recovery");
                if let Err(e) = fabric.thaw().await {
                    tracing::error!(lab = %job.lab_id, error = %e, "failed to thaw fabric on recovery");
                    cleanups.push(Some(LabError::Network(e)));
                } else {
                    state.frozen.store(false, Ordering::SeqCst);
                }
            }
        } else {
            tracing::error!(
                lab = %job.lab_id,
                "VMs remained paused after recovery resume failure; fabric remains frozen to preserve invariant"
            );
        }
    }

    // 3. Clean jail snapshot files
    let jail_res = clean_jail_files_for_vms(&job.vms).await;
    if let Err(e) = jail_res {
        tracing::error!(
            lab = %job.lab_id,
            error = %e,
            "failed to clean jail snapshot files on recovery"
        );
        cleanups.push(Some(e));
    }

    // 4. Clean partial snapshot directory
    let partial = {
        let mut lock = poisoned(&state.partial_snapshot);
        lock.take()
    };
    if let Some(mut snap) = partial {
        let lab_id_clone = job.lab_id.clone();
        let remove_res =
            tokio::task::spawn_blocking(move || snap.remove().map_err(LabError::Store)).await;
        match remove_res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(
                    lab = %job.lab_id,
                    error = %e,
                    "failed to remove partial snapshot directory on recovery"
                );
                cleanups.push(Some(e));
            }
            Err(join_err) => {
                cleanups.push(Some(LabError::Task {
                    lab: lab_id_clone,
                    source: join_err,
                }));
            }
        }
    }

    combine_cleanups(cleanups)
}

#[allow(clippy::too_many_lines)]
async fn run_snapshot_steps(
    job: &SnapshotJob,
    state: &SnapshotExecutionState,
) -> Result<(SnapshotManifest, SnapshotTimings), LabError> {
    let started_all = Instant::now();

    // Step 1: Freeze fabric
    let started_freeze = Instant::now();
    if let Some(fabric) = &job.fabric {
        state.frozen.store(true, Ordering::SeqCst);
        #[cfg(feature = "test-hooks")]
        if let Some(first_vm) = job.vms.first()
            && first_vm.root_dir.join("stall-freeze").exists()
        {
            let _ = std::fs::write(first_vm.root_dir.join("freeze-started"), b"");
            while first_vm.root_dir.join("stall-freeze").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        fabric.freeze().await.map_err(LabError::Network)?;
    }
    let freeze_ms = u64::try_from(started_freeze.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Step 2: Pause all VMs
    let started_pause = Instant::now();
    state.paused.store(true, Ordering::SeqCst);

    let pause_results = join_all(job.vms.iter().map(|vm| async {
        let started = Instant::now();
        let res = vm.client.pause().await;
        (vm.node.clone(), res, started)
    }))
    .await;

    let mut first_pause_err = None;
    for (node, res, _) in pause_results {
        if let Err(err) = res
            && first_pause_err.is_none()
        {
            first_pause_err = Some(LabError::Api {
                lab: job.lab_id.clone(),
                vm: node,
                source: Box::new(err),
            });
        }
    }

    if let Some(pause_err) = first_pause_err {
        return Err(pause_err);
    }
    let pause_ms = u64::try_from(started_pause.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Step 3: Create snapshot directory layout in spawn_blocking
    let store = job.store.clone();
    let lab_id_clone = job.lab_id.clone();
    let snapshot_id_clone = job.snapshot_id.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        store
            .create_snapshot(&lab_id_clone, &snapshot_id_clone)
            .map_err(LabError::Store)
    })
    .await
    .map_err(|join_err| LabError::Task {
        lab: job.lab_id.clone(),
        source: join_err,
    })??;

    {
        let mut lock = poisoned(&state.partial_snapshot);
        *lock = Some(snapshot);
    }

    // Step 3a: Concurrent Firecracker API snapshot requests
    let started_api = Instant::now();
    #[cfg(feature = "test-hooks")]
    if let Some(first_vm) = job.vms.first() {
        trigger_snapshot_hook("api_snapshot", &first_vm.root_dir);
    }

    let mut api_handles = Vec::new();
    for vm in &job.vms {
        let node = vm.node.clone();
        let api = vm.client.clone();
        let lab_id_for_api = job.lab_id.clone();
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

    // Await EVERY dispatched API handle to completion
    let api_results = join_all(api_handles).await;
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
                    if join_err.is_panic() {
                        std::panic::resume_unwind(join_err.into_panic());
                    }
                    first_api_error = Some(LabError::Task {
                        lab: job.lab_id.clone(),
                        source: join_err,
                    });
                }
            }
        }
    }

    if let Some(err) = first_api_error {
        return Err(err);
    }

    // Step 3b: Concurrent owned blocking storage tasks
    let started_storage = Instant::now();
    let snapshot = {
        let mut lock = poisoned(&state.partial_snapshot);
        lock.take().ok_or_else(|| LabError::Invalid {
            lab: job.lab_id.clone(),
            message: "partial snapshot handle missing".into(),
        })?
    };
    let snapshot_arc = Arc::new(snapshot);
    let mut storage_handles = Vec::new();

    for vm in &job.vms {
        let snap = Arc::clone(&snapshot_arc);
        let node = vm.node.clone();
        let state_src = vm.root_dir.join("vm.state");
        let mem_src = vm.root_dir.join("vm.mem");
        let disk_src = vm.disk_path.clone();

        let state_max_bytes = 16 * 1024 * 1024;
        let mem_max_bytes = (vm.memory_mib * 1024 * 1024) + (16 * 1024 * 1024);
        #[cfg(feature = "test-hooks")]
        let root_dir = vm.root_dir.clone();

        let handle = tokio::task::spawn_blocking(move || {
            #[cfg(feature = "test-hooks")]
            if root_dir.join("stall-storage").exists() {
                let _ = std::fs::write(root_dir.join("storage-started"), b"");
                while root_dir.join("stall-storage").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }

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

    // Await EVERY dispatched storage handle to completion
    let storage_results = join_all(storage_handles).await;
    let storage_ms = u64::try_from(started_storage.elapsed().as_millis()).unwrap_or(u64::MAX);

    let mut first_storage_error = None;
    let mut vms_manifest = BTreeMap::new();
    let mut max_fsync_ms = 0u64;
    let mut max_reflink_ms = 0u64;
    let mut max_hash_ms = 0u64;

    for result in storage_results {
        match result {
            Ok(Ok((node, manifest, fsync, reflink, hash))) => {
                vms_manifest.insert(node, manifest);
                max_fsync_ms = max_fsync_ms.max(fsync);
                max_reflink_ms = max_reflink_ms.max(reflink);
                max_hash_ms = max_hash_ms.max(hash);
            }
            Ok(Err(err)) => {
                if first_storage_error.is_none() {
                    first_storage_error = Some(err);
                }
            }
            Err(join_err) => {
                if first_storage_error.is_none() {
                    if join_err.is_panic() {
                        std::panic::resume_unwind(join_err.into_panic());
                    }
                    first_storage_error = Some(LabError::Task {
                        lab: job.lab_id.clone(),
                        source: join_err,
                    });
                }
            }
        }
    }

    let Ok(snapshot) = Arc::try_unwrap(snapshot_arc) else {
        return Err(LabError::Invalid {
            lab: job.lab_id.clone(),
            message: "snapshot handle leaked across storage tasks".into(),
        });
    };

    {
        let mut lock = poisoned(&state.partial_snapshot);
        *lock = Some(snapshot);
    }

    if let Some(err) = first_storage_error {
        return Err(err);
    }

    let snapshot_ms = u64::try_from(started_all.elapsed().as_millis()).unwrap_or(u64::MAX);

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
        lab_spec: job.spec.clone().unwrap_or_else(|| LabSpec {
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
    let snap_for_write = {
        let mut lock = poisoned(&state.partial_snapshot);
        lock.take().ok_or_else(|| LabError::Invalid {
            lab: job.lab_id.clone(),
            message: "snapshot handle missing".into(),
        })?
    };

    let snap_after_write = tokio::task::spawn_blocking(move || {
        snap_for_write.write_manifest(&manifest_clone)?;
        Ok::<_, LabError>(snap_for_write)
    })
    .await
    .map_err(|join_err| LabError::Task {
        lab: job.lab_id.clone(),
        source: join_err,
    })??;

    {
        let mut lock = poisoned(&state.partial_snapshot);
        *lock = Some(snap_after_write);
    }
    let manifest_ms = u64::try_from(started_manifest.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Step 5: Resume all VMs
    let started_resume = Instant::now();
    let resume_results = join_all(job.vms.iter().map(|vm| vm.client.resume())).await;
    let resume_ms = u64::try_from(started_resume.elapsed().as_millis()).unwrap_or(u64::MAX);

    let mut first_resume_err = None;
    for (vm, res) in job.vms.iter().zip(resume_results) {
        if let Err(e) = res
            && first_resume_err.is_none()
        {
            first_resume_err = Some(LabError::Api {
                lab: job.lab_id.clone(),
                vm: vm.node.clone(),
                source: Box::new(e),
            });
        }
    }

    if let Some(resume_err) = first_resume_err {
        return Err(resume_err);
    }
    state.paused.store(false, Ordering::SeqCst);

    // Step 6: Thaw fabric
    let started_thaw = Instant::now();
    if let Some(fabric) = &job.fabric {
        fabric.thaw().await.map_err(LabError::Network)?;
        state.frozen.store(false, Ordering::SeqCst);
    }
    let thaw_ms = u64::try_from(started_thaw.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Step 7: Update step durations in manifest and commit atomically
    manifest.step_durations_ms.resume = resume_ms;
    manifest.step_durations_ms.thaw = thaw_ms;
    let final_manifest = manifest.clone();
    let mut snap_for_commit = {
        let mut lock = poisoned(&state.partial_snapshot);
        lock.take().ok_or_else(|| LabError::Invalid {
            lab: job.lab_id.clone(),
            message: "snapshot handle missing".into(),
        })?
    };

    tokio::task::spawn_blocking(move || {
        snap_for_commit.write_manifest(&final_manifest)?;
        snap_for_commit.commit();
        Ok::<_, LabError>(())
    })
    .await
    .map_err(|join_err| LabError::Task {
        lab: job.lab_id.clone(),
        source: join_err,
    })??;

    // Clean jail snapshot files on success as well
    if let Err(e) = clean_jail_files_for_vms(&job.vms).await {
        tracing::warn!(lab = %job.lab_id, error = %e, "warning cleaning jail files after snapshot");
    }

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
        lab = %job.lab_id,
        snapshot = %job.snapshot_id,
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

async fn run_snapshot_job(
    job: SnapshotJob,
) -> Result<(SnapshotManifest, SnapshotTimings), LabError> {
    let state = SnapshotExecutionState {
        frozen: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        partial_snapshot: Mutex::new(None),
    };

    let outcome = std::panic::AssertUnwindSafe(run_snapshot_steps(&job, &state))
        .catch_unwind()
        .await;

    match outcome {
        Ok(Ok((manifest, timings))) => Ok((manifest, timings)),
        Ok(Err(op_err)) => {
            let cleanup_err = run_snapshot_recovery(&job, &state).await;
            Err(combine_rollback(op_err, cleanup_err))
        }
        Err(panic_payload) => {
            tracing::error!(lab = %job.lab_id, "snapshot task panicked, running recovery");
            let cleanup_err = run_snapshot_recovery(&job, &state).await;
            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let panic_err = LabError::Invalid {
                lab: job.lab_id.clone(),
                message: format!("snapshot task panicked: {panic_msg}"),
            };
            Err(combine_rollback(panic_err, cleanup_err))
        }
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
/// Spawns an owned run-to-completion task that the caller cannot cancel midway.
/// Dropping the caller's future leaves the task running safely to completion and recovery.
///
/// # Errors
/// Returns [`LabError`] on failure at any step or rollback.
pub async fn snapshot_lab_with_timings(
    lab: &mut Lab,
    snapshot_id: &str,
) -> Result<(SnapshotManifest, SnapshotTimings), LabError> {
    let lab_id = lab.id().to_string();
    dylos_core::validate_name(snapshot_id, 32)
        .map_err(|e| LabError::Store(dylos_store::Error::InvalidId(e.to_string())))?;

    let tracker = lab.active_snapshot_tracker();
    {
        let mut lock = tracker.lock().await;
        if let Some(handle) = lock.as_ref() {
            if handle.is_finished() {
                *lock = None;
            } else {
                return Err(LabError::Invalid {
                    lab: lab_id,
                    message: "snapshot already in progress".into(),
                });
            }
        }
    }

    let store = lab.store().cloned().unwrap_or_default();
    let spec = lab.spec().cloned();
    let storage = lab.storage();
    let fabric = lab.network().map(dylos_net::LabNetwork::thaw_handle);

    let mut vms = Vec::new();
    for vm in lab.vms() {
        let node = vm.node().to_string();
        let client = vm.client().clone();
        let root_dir = vm.paths().root.clone();
        let disk_path = storage
            .map(|s| s.vm_paths(&node).map(|p| p.rootfs))
            .transpose()?;
        let mem_mib = spec
            .as_ref()
            .and_then(|s| s.nodes.iter().find(|n| n.name == node))
            .map_or(256, |n| u64::from(n.memory));
        vms.push(SnapshotVm {
            node,
            client,
            root_dir,
            disk_path,
            memory_mib: mem_mib,
        });
    }

    let job = SnapshotJob {
        lab_id: lab_id.clone(),
        snapshot_id: snapshot_id.to_string(),
        spec,
        store,
        vms,
        fabric,
    };

    let span = tracing::info_span!("snapshot_lab", lab = %lab_id, snapshot = %snapshot_id);
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(
        async move {
            let res = run_snapshot_job(job).await;
            let _ = result_tx.send(res);
        }
        .instrument(span),
    );

    {
        let mut lock = tracker.lock().await;
        *lock = Some(task);
    }

    let result = if let Ok(res) = result_rx.await {
        res
    } else {
        // Task completed or panicked without sending via oneshot
        let mut lock = tracker.lock().await;
        if let Some(handle) = lock.as_mut() {
            match handle.await {
                Ok(()) => Err(LabError::Invalid {
                    lab: lab_id,
                    message: "snapshot task finished without publishing result".into(),
                }),
                Err(join_err) => {
                    if join_err.is_panic() {
                        Err(LabError::Invalid {
                            lab: lab_id,
                            message: "snapshot task panicked".into(),
                        })
                    } else {
                        Err(LabError::Task {
                            lab: lab_id,
                            source: join_err,
                        })
                    }
                }
            }
        } else {
            Err(LabError::Invalid {
                lab: lab_id,
                message: "snapshot task disappeared".into(),
            })
        }
    };

    {
        let mut lock = tracker.lock().await;
        *lock = None;
    }

    result
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

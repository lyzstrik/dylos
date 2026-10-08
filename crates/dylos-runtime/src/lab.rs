use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use dylos_core::{LabSpec, guest_net};
use dylos_fc::config::{BootSource, Drive, MachineConfiguration, NetworkInterface};
use dylos_net::{FabricPlan, LabNetwork};
use futures_util::future::join_all;
use nix::fcntl::{Flock, FlockArg, OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::Instrument;

use crate::jailer::{ChrootFile, JailPaths, JailerConfig};
use crate::{ProcessState, Timeouts, Vm, VmSpec};

#[derive(Debug, thiserror::Error)]
pub enum LabError {
    #[error("lab {lab}: {message}")]
    Invalid { lab: String, message: String },
    #[error("lab {lab}: {source}")]
    Io {
        lab: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Spec(#[from] dylos_core::Error),
    #[error(transparent)]
    Store(#[from] dylos_store::Error),
    #[error(transparent)]
    Network(#[from] dylos_net::Error),
    #[error(transparent)]
    Vm(#[from] crate::Error),
    #[error("lab {lab}, VM {vm}: {source}")]
    Api {
        lab: String,
        vm: String,
        #[source]
        source: Box<dylos_fc::Error>,
    },
    #[error("lab {lab}: blocking task failed: {source}")]
    Task {
        lab: String,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("lab operation failed: {operation}; rollback failed: {cleanup}")]
    Rollback {
        operation: Box<Self>,
        cleanup: Box<Self>,
    },
}

type Result<T> = std::result::Result<T, LabError>;

/// Paths must be operator-controlled and not writable by the jailer uid or other users.
#[derive(Debug, Clone)]
pub struct LabConfig {
    pub store: dylos_store::Store,
    pub jailer: JailerConfig,
    pub kernel: PathBuf,
    pub netns_dir: PathBuf,
    pub timeouts: Timeouts,
    pub boot_timeout: Duration,
}

impl Default for LabConfig {
    fn default() -> Self {
        Self {
            store: dylos_store::Store::default(),
            jailer: JailerConfig::new("/srv/jailer", 65534, 65534),
            kernel: "kernels/vmlinux.bin".into(),
            netns_dir: "/run/netns".into(),
            timeouts: Timeouts::default(),
            boot_timeout: Duration::from_secs(30),
        }
    }
}

/// Keeps the lab alive in the process that called `up`. Explicit teardown observes errors.
#[must_use]
pub struct Lab {
    id: String,
    storage: Option<dylos_store::Lab>,
    network: Option<LabNetwork>,
    vms: Vec<Vm>,
    listener: Option<UnixListener>,
    lock: Option<Flock<File>>,
    span: tracing::Span,
    cleanup_on_drop: bool,
}

impl Lab {
    /// # Errors
    /// Rejects invalid specs and rolls back all completed steps on a launch or readiness error.
    pub async fn up(spec: &LabSpec, lab_id: &str) -> Result<Self> {
        Self::up_with(&LabConfig::default(), spec, lab_id).await
    }

    /// # Errors
    /// As `up`, with operator-supplied host paths and timeouts.
    pub async fn up_with(config: &LabConfig, spec: &LabSpec, lab_id: &str) -> Result<Self> {
        validate(config, spec, lab_id)?;
        let plan = FabricPlan::new(lab_id, spec)?;
        let span = tracing::info_span!("lab", lab = lab_id);
        async {
            let started = Instant::now();
            let (store, spec_copy, id) = (config.store.clone(), spec.clone(), lab_id.to_owned());
            let storage = blocking(lab_id, move || Ok(store.create(&id, &spec_copy)?)).await?;
            let mut lab = Self {
                id: lab_id.into(),
                storage: Some(storage),
                network: None,
                vms: Vec::new(),
                listener: None,
                lock: None,
                span: span.clone(),
                cleanup_on_drop: true,
            };
            let result = lab.bring_up(config, spec, plan).await;
            if let Err(operation) = result {
                return match lab.teardown().await {
                    Ok(()) => Err(operation),
                    Err(cleanup) => Err(LabError::Rollback {
                        operation: Box::new(operation),
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
            tracing::info!(duration_ms = started.elapsed().as_millis(), "lab ready");
            Ok(lab)
        }
        .instrument(span.clone())
        .await
    }

    async fn bring_up(
        &mut self,
        config: &LabConfig,
        spec: &LabSpec,
        plan: FabricPlan,
    ) -> Result<()> {
        let root = self.root().to_owned();
        self.lock = Some(blocking(&self.id, move || lock_directory(&root)).await?);
        self.network = Some(create_network(config, plan).await?);
        let netns = config.netns_dir.join(format!("dylos-{}", self.id));
        let specs: Vec<_> = spec
            .nodes
            .iter()
            .map(|node| {
                Ok(VmSpec {
                    lab_id: jail_identity(&self.id),
                    node: node.name.clone(),
                    vcpu_count: u8::try_from(node.vcpus)
                        .map_err(|_| invalid(&self.id, "vCPU count exceeds u8"))?,
                    mem_size_mib: node.memory,
                    netns: Some(netns.clone()),
                    files: vec![
                        ChrootFile::new(&config.kernel, "vmlinux.bin"),
                        ChrootFile::new(
                            self.root().join("vms").join(&node.name).join("rootfs.ext4"),
                            "rootfs.ext4",
                        ),
                    ],
                    timeouts: config.timeouts,
                })
            })
            .collect::<Result<_>>()?;
        let started = Instant::now();
        let results = join_all(specs.iter().map(|vm| Vm::launch(&config.jailer, vm))).await;
        let mut failure = None;
        for result in results {
            match result {
                Ok(vm) => self.vms.push(vm),
                Err(error) => {
                    tracing::error!(%error, "VM launch failed");
                    failure.get_or_insert(LabError::Vm(error));
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        tracing::info!(
            duration_ms = started.elapsed().as_millis(),
            "all VM APIs ready"
        );
        let started = Instant::now();
        complete(
            join_all(
                self.vms
                    .iter()
                    .zip(&spec.nodes)
                    .map(|(vm, node)| configure_vm(&self.id, spec, node, vm)),
            )
            .await,
        )?;
        tracing::info!(
            duration_ms = started.elapsed().as_millis(),
            "all VMs configured"
        );
        complete(
            join_all(self.vms.iter().map(|vm| async {
                vm.client()
                    .start_instance()
                    .await
                    .map_err(|source| LabError::Api {
                        lab: self.id.clone(),
                        vm: vm.paths().id.clone(),
                        source: Box::new(source),
                    })
            }))
            .await,
        )?;
        complete(
            join_all(self.vms.iter().map(|vm| async {
                vm.wait_for_ready(config.boot_timeout)
                    .await
                    .map(|_| ())
                    .map_err(LabError::from)
            }))
            .await,
        )?;
        let socket = self.control_socket();
        self.listener = Some(UnixListener::bind(&socket).map_err(|e| io(&self.id, e))?);
        tokio::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| io(&self.id, e))?;
        Ok(())
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.storage
            .as_ref()
            .map_or(Path::new(""), dylos_store::Lab::root)
    }

    #[must_use]
    pub fn control_socket(&self) -> PathBuf {
        self.root().join("control.sock")
    }

    #[must_use]
    pub fn vms(&self) -> &[Vm] {
        &self.vms
    }

    /// Serves root-only teardown requests and tears down if any VM exits unexpectedly.
    /// # Errors
    /// Reports socket, VM exit, and cleanup failures. A response is sent only after cleanup.
    pub async fn supervise(mut self) -> Result<()> {
        let result = async {
            let listener = self.listener.as_ref().ok_or_else(|| invalid(&self.id, "lab already stopped"))?;
            let exits = futures_util::stream::FuturesUnordered::new();
            for vm in &self.vms {
                let mut state = vm.state();
                exits.push(async move {
                    let _exit = state.wait_for(|s| *s != ProcessState::Running).await;
                });
            }
            tokio::pin!(exits);
            loop {
                use futures_util::StreamExt;
                tokio::select! {
                    _ = exits.next(), if !self.vms.is_empty() => return Err(invalid(&self.id, "VM exited unexpectedly")),
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.map_err(|e| io(&self.id, e))?;
                        let credentials = stream.peer_cred().map_err(|e| io(&self.id, e))?;
                        if credentials.uid() != nix::unistd::geteuid().as_raw() { continue; }
                        let mut request = [0; 9];
                        if matches!(tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut request)).await, Ok(Ok(_))) && &request == b"teardown\n" {
                            return Ok(stream);
                        }
                    }
                }
            }
        }.instrument(self.span.clone()).await;
        let cleanup = self.teardown().await;
        match (result, cleanup) {
            (Ok(mut stream), Ok(())) => {
                stream.write_all(b"ok\n").await.map_err(|e| io(&self.id, e))
            }
            (Err(operation), Err(cleanup)) => Err(LabError::Rollback {
                operation: Box::new(operation),
                cleanup: Box::new(cleanup),
            }),
            (_, Err(error)) | (Err(error), _) => Err(error),
        }
    }

    /// Stops all VMs before removing fabric and storage; safe to call twice.
    /// # Errors
    /// Cleanup failures preserve remaining owned resources for a retry.
    pub async fn teardown(&mut self) -> Result<()> {
        async {
            let started = Instant::now();
            complete(
                join_all(
                    self.vms
                        .iter_mut()
                        .map(|vm| async { vm.shutdown().await.map_err(LabError::from) }),
                )
                .await,
            )?;
            self.vms.clear();
            if let Some(net) = &mut self.network {
                net.teardown().await?;
            }
            self.network = None;
            self.listener = None;
            if let Some(mut storage) = self.storage.take() {
                let (storage_back, result) = tokio::task::spawn_blocking(move || {
                    let result = storage.remove();
                    (storage, result)
                })
                .await
                .map_err(|source| LabError::Task {
                    lab: self.id.clone(),
                    source,
                })?;
                self.storage = Some(storage_back);
                result?;
                self.storage = None;
            }
            self.lock = None;
            tracing::info!(duration_ms = started.elapsed().as_millis(), "lab removed");
            Ok(())
        }
        .instrument(self.span.clone())
        .await
    }
}

fn complete(results: Vec<Result<()>>) -> Result<()> {
    let mut failure = None;
    for result in results {
        if let Err(error) = result {
            tracing::error!(%error, "lab step failed");
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

fn invalid(lab: &str, message: &str) -> LabError {
    LabError::Invalid {
        lab: lab.into(),
        message: message.into(),
    }
}
fn io(lab: &str, source: std::io::Error) -> LabError {
    LabError::Io {
        lab: lab.into(),
        source,
    }
}

fn validate(config: &LabConfig, spec: &LabSpec, id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid(id, "invalid lab id"));
    }
    if !cfg!(feature = "test-hooks")
        && (nix::unistd::geteuid().as_raw() != 0
            || config.jailer.uid == 0
            || config.jailer.gid == 0)
    {
        return Err(invalid(
            id,
            "requires root orchestrator and non-root jailer uid/gid",
        ));
    }
    spec.validate()?;
    for node in &spec.nodes {
        if node.vcpus == 0
            || node.vcpus > 32
            || (node.vcpus != 1 && node.vcpus % 2 != 0)
            || node.memory == 0
        {
            return Err(invalid(id, "invalid VM vCPU or memory configuration"));
        }
        JailPaths::new(&config.jailer, &jail_identity(id), &node.name)?;
        guest_net::boot_args(spec, node)?;
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    lab: &str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|source| LabError::Task {
            lab: lab.into(),
            source,
        })?
}

fn open_directory(path: &Path, create: bool) -> std::io::Result<File> {
    let mut file = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                if create {
                    match mkdirat(&file, name, Mode::from_bits_truncate(0o755)) {
                        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                file = File::from(openat(
                    &file,
                    name,
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )?);
            }
            Component::RootDir | Component::CurDir => {}
            _ => return Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
    Ok(file)
}

fn lock_directory(path: &Path) -> Result<Flock<File>> {
    let lab = path.to_string_lossy();
    let file = open_directory(path, false).map_err(|e| io(&lab, e))?;
    let metadata = file.metadata().map_err(|e| io(&lab, e))?;
    if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(invalid(
            &lab,
            "lab directory must be private and owned by the orchestrator",
        ));
    }
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| io(&lab, e.into()))
}

impl Lab {
    /// Requests teardown and waits for confirmation that resources were removed.
    /// # Errors
    /// Connection, timeout or protocol failure. A timeout leaves the server outcome unknown.
    pub async fn request_teardown(socket: &Path, timeout: Duration) -> Result<()> {
        let id = socket.to_string_lossy();
        tokio::time::timeout(timeout, async {
            let mut stream = UnixStream::connect(socket).await.map_err(|e| io(&id, e))?;
            stream
                .write_all(b"teardown\n")
                .await
                .map_err(|e| io(&id, e))?;
            let mut reply = [0; 3];
            stream
                .read_exact(&mut reply)
                .await
                .map_err(|e| io(&id, e))?;
            if &reply != b"ok\n" {
                return Err(invalid(&id, "invalid teardown response"));
            }
            Ok(())
        })
        .await
        .map_err(|_| io(&id, std::io::ErrorKind::TimedOut.into()))?
    }

    /// Cleans a crashed supervisor's lab by name, using its persisted spec, never a saved PID.
    /// A live supervisor holds an exclusive directory lock and cannot be bypassed.
    /// # Errors
    /// Refuses invalid ids, unsafe state paths, live supervisors or populated cgroups that
    /// cannot be killed. Config must match the configuration used at bring-up.
    pub async fn teardown_by_name(config: &LabConfig, id: &str) -> Result<()> {
        validate(
            config,
            &LabSpec {
                nodes: vec![],
                segments: vec![],
            },
            id,
        )?;
        let root = config.store.base.join(id);
        let (root_copy, id_copy) = (root.clone(), id.to_owned());
        let recovered = blocking(id, move || {
            let lock = match lock_directory(&root_copy) {
                Err(LabError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(None);
                }
                result => result?,
            };
            let file = match openat(
                &*lock,
                "state.json",
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => File::from(file),
                Err(nix::errno::Errno::ENOENT) => return Ok(Some((lock, None))),
                Err(error) => return Err(io(&id_copy, error.into())),
            };
            if !file.metadata().map_err(|e| io(&id_copy, e))?.is_file() {
                return Err(invalid(&id_copy, "state is not a regular file"));
            }
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| io(&id_copy, e))?;
            if bytes.len() > 1024 * 1024 {
                return Err(invalid(&id_copy, "state exceeds 1 MiB"));
            }
            let state: dylos_store::State = serde_json::from_slice(&bytes)
                .map_err(|e| invalid(&id_copy, &format!("invalid state: {e}")))?;
            if state.lab_id != id_copy {
                return Err(invalid(&id_copy, "state lab id mismatch"));
            }
            Ok(Some((lock, Some(state.spec))))
        })
        .await?;
        let Some((_lock, spec)) = recovered else {
            return Ok(());
        };
        if let Some(spec) = spec {
            validate(config, &spec, id)?;
            let plan = FabricPlan::new(id, &spec)?;
            // cgroup.kill addresses the owned group, avoiding recycled host PID races.
            for node in &spec.nodes {
                let paths = JailPaths::new(&config.jailer, &jail_identity(id), &node.name)?;
                let (cgroup, lab) = (paths.cgroup_dir.clone(), id.to_owned());
                blocking(id, move || stop_cgroup(&lab, &cgroup)).await?;
                crate::jailer::remove_jail(&paths).await?;
            }
            dylos_net::teardown(&plan, &config.netns_dir).await?;
        }
        let (store, id_copy) = (config.store.clone(), id.to_owned());
        blocking(id, move || Ok(store.remove(&id_copy)?)).await
    }
}

fn stop_cgroup(id: &str, path: &Path) -> Result<()> {
    let dir = match open_directory(path, false) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        result => result.map_err(|e| io(id, e))?,
    };
    let open = |name: &str, flags| {
        openat(
            &dir,
            name,
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|e| io(id, e.into()))
    };
    let events = || {
        let mut text = String::new();
        match open("cgroup.events", OFlag::O_RDONLY) {
            Err(LabError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound
                    && cfg!(feature = "test-hooks") =>
            {
                return Ok(false);
            }
            result => {
                result?
                    .take(4096)
                    .read_to_string(&mut text)
                    .map_err(|e| io(id, e))?;
            }
        }
        if text.lines().any(|line| line == "populated 0") {
            Ok(false)
        } else if text.lines().any(|line| line == "populated 1") {
            Ok(true)
        } else {
            Err(invalid(id, "invalid cgroup.events"))
        }
    };
    if events()? {
        open("cgroup.kill", OFlag::O_WRONLY)?
            .write_all(b"1")
            .map_err(|e| io(id, e))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while events()? {
            if Instant::now() >= deadline {
                return Err(invalid(id, "cgroup remained populated after kill"));
            }
            std::thread::yield_now();
        }
    }
    Ok(())
}

async fn configure_vm(id: &str, spec: &LabSpec, node: &dylos_core::Node, vm: &Vm) -> Result<()> {
    let vcpus = u8::try_from(node.vcpus).map_err(|_| invalid(id, "invalid vCPU count"))?;
    let boot_args = guest_net::boot_args(spec, node)?;
    async {
        let api = vm.client();
        let configure = async {
            api.put_machine_config(&MachineConfiguration::new(vcpus, node.memory as usize))
                .await?;
            let mut boot = BootSource::new("/vmlinux.bin");
            boot.boot_args = Some(format!(
                "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda {boot_args}"
            ));
            api.put_boot_source(&boot).await?;
            let mut drive = Drive::new("rootfs", true);
            drive.path_on_host = Some("/rootfs.ext4".into());
            drive.is_read_only = Some(false);
            api.put_drive(&drive).await?;
            for interface in &node.interfaces {
                let mut net = NetworkInterface::new(
                    &interface.name,
                    format!("tap-{}-{}", node.name, interface.name),
                );
                net.guest_mac =
                    Some(guest_net::mac_for_interface(&node.name, &interface.name).to_string());
                api.put_network_interface(&net).await?;
            }
            Ok::<_, dylos_fc::Error>(())
        };
        configure.await.map_err(|source| LabError::Api {
            lab: id.into(),
            vm: node.name.clone(),
            source: Box::new(source),
        })
    }
    .instrument(tracing::info_span!("vm_configure", vm = %node.name))
    .await
}

impl Drop for Lab {
    fn drop(&mut self) {
        if !self.cleanup_on_drop || self.storage.is_none() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let mut owned = Self {
                id: self.id.clone(),
                storage: self.storage.take(),
                network: self.network.take(),
                vms: std::mem::take(&mut self.vms),
                listener: self.listener.take(),
                lock: self.lock.take(),
                span: self.span.clone(),
                cleanup_on_drop: false,
            };
            runtime.spawn(async move {
                if let Err(error) = owned.teardown().await {
                    tracing::error!(lab = %owned.id, %error, "lab cleanup on drop failed");
                }
            });
        } else {
            tracing::error!(lab = %self.id, "lab dropped outside runtime; explicit recovery by name may be needed");
        }
    }
}

// Length-prefix the lab id: lab `a-b`, node `c` must not alias lab `a`, node `b-c`.
fn jail_identity(id: &str) -> String {
    format!("lab{}-{id}", id.len())
}

async fn create_network(config: &LabConfig, plan: FabricPlan) -> Result<LabNetwork> {
    let started = Instant::now();
    let netns_dir = config.netns_dir.clone();
    let lab_id = plan.lab_id().to_owned();
    blocking(plan.lab_id(), move || {
        open_directory(&netns_dir, true)
            .map(|_| ())
            .map_err(|e| io(&lab_id, e))
    })
    .await?;
    let network = LabNetwork::create(
        plan,
        &config.netns_dir,
        config.jailer.uid,
        config.jailer.gid,
    )
    .await?;
    tracing::info!(duration_ms = started.elapsed().as_millis(), "fabric ready");
    Ok(network)
}

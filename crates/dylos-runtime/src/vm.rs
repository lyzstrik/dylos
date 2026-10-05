use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dylos_fc::FcClient;
use dylos_fc::config::{ActionType, InstanceActionInfo};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::error::{Error, Result};
use crate::jailer::{self, ChrootFile, JailPaths, JailerConfig, METRICS_FILE};

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// From spawn to the first successful API call.
    pub ready: Duration,
    /// After `SendCtrlAltDel`, before SIGTERM.
    pub graceful: Duration,
    /// After SIGTERM, before SIGKILL.
    pub term: Duration,
    /// After SIGKILL, before giving up.
    pub kill: Duration,
    /// How often the metrics file is read into `tracing`.
    pub metrics_poll: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            ready: Duration::from_secs(5),
            graceful: Duration::from_secs(10),
            term: Duration::from_secs(3),
            kill: Duration::from_secs(2),
            metrics_poll: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VmSpec {
    pub lab_id: String,
    pub node: String,
    /// Network namespace the jailer joins (`--netns`); `None` keeps the caller's.
    pub netns: Option<PathBuf>,
    pub files: Vec<ChrootFile>,
    pub timeouts: Timeouts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Exited(ExitStatus),
    /// `wait` itself failed; the process was sent SIGKILL and its status is unknown.
    Lost,
}

/// A running Firecracker process inside its jail.
///
/// [`Vm::shutdown`] is the normal way to stop it. `Drop` is only a safety net: it cannot wait, so
/// it aborts the supervisor (the child is spawned with `kill_on_drop`, so it gets SIGKILL and is
/// reaped by tokio in the background) and removes the jail directory synchronously. The cgroup
/// is left behind if the process has not been reaped yet.
pub struct Vm {
    paths: JailPaths,
    client: FcClient,
    timeouts: Timeouts,
    signals: mpsc::UnboundedSender<Signal>,
    state: watch::Receiver<ProcessState>,
    expected_exit: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
    pid: Option<u32>,
    cleaned_up: bool,
}

impl Vm {
    /// Prepares the chroot, starts the jailer and waits until the Firecracker API answers.
    /// On any error, the process is killed and the jail removed before returning.
    ///
    /// # Errors
    ///
    /// Any [`Error`] from chroot preparation, [`Error::Spawn`], [`Error::ExitedDuringStart`] or
    /// [`Error::ReadyTimeout`].
    pub async fn launch(config: &JailerConfig, spec: &VmSpec) -> Result<Self> {
        let paths = JailPaths::new(config, &spec.lab_id, &spec.node)?;
        let span = tracing::info_span!("vm", lab = %spec.lab_id, node = %spec.node);
        async {
            let start = std::time::Instant::now();
            jailer::prepare_chroot(config, &paths, &spec.files).await?;
            let mut vm = match Self::spawn(config, spec, paths.clone()) {
                Ok(vm) => vm,
                Err(e) => {
                    if let Err(cleanup) = jailer::remove_jail(&paths).await {
                        tracing::error!(error = %cleanup, "cleanup after failed spawn");
                    }
                    return Err(e);
                }
            };
            if let Err(e) = vm.wait_ready().await {
                vm.kill_and_clean_up().await;
                return Err(e);
            }
            tracing::info!(duration_ms = start.elapsed().as_millis(), "VM API ready");
            Ok(vm)
        }
        .instrument(span)
        .await
    }

    fn spawn(config: &JailerConfig, spec: &VmSpec, paths: JailPaths) -> Result<Self> {
        let mut child = Command::new(&config.jailer)
            .args(jailer::jailer_args(config, &paths, spec.netns.as_deref()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| Error::Spawn {
                id: paths.id.clone(),
                program: config.jailer.clone(),
                source,
            })?;
        let pid = child.id();
        let mut tasks = Vec::new();
        // Without `--log-path`, Firecracker logs to stdout; the jailer keeps our pipes since it
        // is not daemonized.
        if let Some(out) = child.stdout.take() {
            tasks.push(tokio::spawn(forward_lines(out, "stdout").in_current_span()));
        }
        if let Some(err) = child.stderr.take() {
            tasks.push(tokio::spawn(forward_lines(err, "stderr").in_current_span()));
        }
        let (signals, signal_rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(ProcessState::Running);
        let expected_exit = Arc::new(AtomicBool::new(false));
        let supervisor = supervise(child, signal_rx, state_tx, Arc::clone(&expected_exit));
        tasks.push(tokio::spawn(supervisor.in_current_span()));
        let metrics = tail_metrics(
            paths.host_path(METRICS_FILE),
            state.clone(),
            spec.timeouts.metrics_poll,
        );
        tasks.push(tokio::spawn(metrics.in_current_span()));
        Ok(Self {
            client: FcClient::new(paths.api_socket()),
            paths,
            timeouts: spec.timeouts,
            signals,
            state,
            expected_exit,
            tasks,
            pid,
            cleaned_up: false,
        })
    }

    #[must_use]
    pub fn client(&self) -> &FcClient {
        &self.client
    }

    /// Pid of the jailer, which becomes Firecracker after `exec`.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    #[must_use]
    pub fn paths(&self) -> &JailPaths {
        &self.paths
    }

    /// Changes once, when the process exits for any reason. An exit that was not requested
    /// through [`Vm::shutdown`] is also logged as an error.
    #[must_use]
    pub fn state(&self) -> watch::Receiver<ProcessState> {
        self.state.clone()
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let poll = async {
            let mut state = self.state.clone();
            loop {
                let current = state.borrow_and_update().clone();
                if current != ProcessState::Running {
                    return Err(Error::ExitedDuringStart {
                        id: self.paths.id.clone(),
                        state: current,
                    });
                }
                // GET / returns the instance info once the API thread serves requests.
                if self.client.get::<serde_json::Value>("/").await.is_ok() {
                    return Ok(());
                }
                // Not a fixed delay: retry soon, or at once if the process exits.
                let _ = tokio::time::timeout(Duration::from_millis(10), state.changed()).await;
            }
        };
        tokio::time::timeout(self.timeouts.ready, poll)
            .await
            .unwrap_or_else(|_| {
                Err(Error::ReadyTimeout {
                    id: self.paths.id.clone(),
                    socket: self.paths.api_socket(),
                    timeout: self.timeouts.ready,
                })
            })
    }

    async fn wait_exit(&self, timeout: Duration) -> bool {
        let mut state = self.state.clone();
        tokio::time::timeout(timeout, state.wait_for(|s| *s != ProcessState::Running))
            .await
            .is_ok()
    }

    /// Stops the VM: `SendCtrlAltDel`, then SIGTERM, then SIGKILL, each after its timeout, then
    /// removes the jail. Idempotent.
    ///
    /// # Errors
    ///
    /// [`Error::KillTimeout`] if the process survives SIGKILL (the jail is kept, since the
    /// process may still use it), or [`Error::Io`] if cleanup fails.
    pub async fn shutdown(&mut self) -> Result<()> {
        let span = tracing::info_span!("vm_shutdown", vm = %self.paths.id);
        async {
            let start = std::time::Instant::now();
            self.expected_exit.store(true, Ordering::SeqCst);
            let ctrl_alt_del = InstanceActionInfo::new(ActionType::SendCtrlAltDel);
            let graceful = *self.state.borrow() == ProcessState::Running
                && match self
                    .client
                    .put::<_, serde_json::Value>("/actions", &ctrl_alt_del)
                    .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::debug!(error = %e, "SendCtrlAltDel refused, skipping to SIGTERM");
                        false
                    }
                };
            if !(graceful && self.wait_exit(self.timeouts.graceful).await) {
                let _ = self.signals.send(Signal::SIGTERM);
                if !self.wait_exit(self.timeouts.term).await {
                    tracing::warn!("Firecracker ignored SIGTERM, sending SIGKILL");
                    let _ = self.signals.send(Signal::SIGKILL);
                    if !self.wait_exit(self.timeouts.kill).await {
                        return Err(Error::KillTimeout {
                            id: self.paths.id.clone(),
                            timeout: self.timeouts.kill,
                        });
                    }
                }
            }
            for task in self.tasks.drain(..) {
                let _ = task.await;
            }
            jailer::remove_jail(&self.paths).await?;
            self.cleaned_up = true;
            tracing::info!(
                duration_ms = start.elapsed().as_millis(),
                "VM stopped and cleaned up"
            );
            Ok(())
        }
        .instrument(span)
        .await
    }

    async fn kill_and_clean_up(&mut self) {
        self.expected_exit.store(true, Ordering::SeqCst);
        let _ = self.signals.send(Signal::SIGKILL);
        if !self.wait_exit(self.timeouts.kill).await {
            tracing::error!(vm = %self.paths.id, "process survived SIGKILL during failed start");
            return;
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        match jailer::remove_jail(&self.paths).await {
            Ok(()) => self.cleaned_up = true,
            Err(e) => {
                tracing::error!(vm = %self.paths.id, error = %e, "cleanup after failed start");
            }
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        tracing::warn!(vm = %self.paths.id, "Vm dropped without shutdown, killing it");
        self.expected_exit.store(true, Ordering::SeqCst);
        for task in &self.tasks {
            task.abort();
        }
        // Blocking, but only on this fallback path. Unlinking is safe even if Firecracker is
        // still running: it keeps its open files until SIGKILL lands.
        if let Err(e) = std::fs::remove_dir_all(&self.paths.jail_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(vm = %self.paths.id, error = %e, "failed to remove jail directory");
        }
    }
}

/// Owns the child: it is the only place that waits on (reaps) it, so a signal sent from here can
/// never reach a recycled pid.
async fn supervise(
    mut child: Child,
    mut signals: mpsc::UnboundedReceiver<Signal>,
    state: watch::Sender<ProcessState>,
    expected_exit: Arc<AtomicBool>,
) {
    let pid = child
        .id()
        .and_then(|p| i32::try_from(p).ok())
        .map(Pid::from_raw);
    let new_state = loop {
        tokio::select! {
            res = child.wait() => break match res {
                Ok(status) => ProcessState::Exited(status),
                Err(e) => {
                    tracing::error!(error = %e, "waiting on Firecracker failed, killing it");
                    let _ = child.start_kill();
                    ProcessState::Lost
                }
            },
            Some(signal) = signals.recv() => {
                let res = match (signal, pid) {
                    (Signal::SIGKILL, _) | (_, None) => child.start_kill(),
                    (signal, Some(pid)) => kill(pid, signal).map_err(std::io::Error::from),
                };
                if let Err(e) = res {
                    tracing::warn!(%signal, error = %e, "failed to signal Firecracker");
                }
            }
        }
    };
    if expected_exit.load(Ordering::SeqCst) {
        tracing::info!(state = ?new_state, "Firecracker exited");
    } else {
        tracing::error!(state = ?new_state, "Firecracker exited unexpectedly");
    }
    state.send_replace(new_state);
}

async fn forward_lines(stream: impl AsyncRead + Unpin, source: &'static str) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => tracing::info!(target: "dylos::firecracker", source, "{line}"),
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(source, error = %e, "failed to read Firecracker output");
                break;
            }
        }
    }
}

/// Firecracker appends one JSON object per flush to the metrics file (every 60 s, and on
/// `FlushMetrics`). Reads what was appended since the last poll, and once more after exit.
async fn tail_metrics(path: PathBuf, mut state: watch::Receiver<ProcessState>, every: Duration) {
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot open metrics file");
            return;
        }
    };
    let mut pending = String::new();
    let mut interval = tokio::time::interval(every);
    loop {
        let exited = tokio::select! {
            _ = interval.tick() => false,
            _ = state.wait_for(|s| *s != ProcessState::Running) => true,
        };
        if let Err(e) = file.read_to_string(&mut pending).await {
            tracing::warn!(error = %e, "failed to read metrics");
        }
        while let Some(end) = pending.find('\n') {
            let line: String = pending.drain(..=end).collect();
            if !line.trim().is_empty() {
                tracing::info!(target: "dylos::firecracker::metrics", "{}", line.trim_end());
            }
        }
        if exited {
            return;
        }
    }
}

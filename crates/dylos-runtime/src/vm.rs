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
use crate::jailer::{self, ChrootFile, Jail, JailPaths, JailerConfig, METRICS_FILE};

/// Delay between readiness probes while the API socket is not answering yet.
const READY_RETRY: Duration = Duration::from_millis(10);

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
    /// How often the metrics file is read into `tracing`. Must not be zero.
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
    pub vcpu_count: u8,
    /// Guest RAM in MiB; must match the Firecracker machine configuration.
    pub mem_size_mib: u32,
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
/// The supervisor task owns the child and the [`Jail`]: whatever the reason the process exits,
/// it reaps it, then removes the cgroup and the jail. [`Vm::shutdown`] escalates signals and
/// waits for that. Dropping a `Vm` without shutdown closes the signal channel, which tells the
/// supervisor to SIGKILL the process and clean up in the background. If the runtime shuts down
/// before that, the child gets SIGKILL from `kill_on_drop` and the jail's drop removes what it can
/// synchronously; a cgroup whose process is not reaped yet stays behind.
pub struct Vm {
    paths: JailPaths,
    client: FcClient,
    timeouts: Timeouts,
    signals: mpsc::UnboundedSender<Signal>,
    state: watch::Receiver<ProcessState>,
    expected_exit: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<Result<()>>>,
    tasks: Vec<JoinHandle<()>>,
    cleanup_error: Option<String>,
    pid: Option<u32>,
    output: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    output_tasks_done: mpsc::Receiver<()>,
}

impl Vm {
    /// Prepares the chroot, starts the jailer and waits until the Firecracker API answers.
    /// On any error, the process is killed and the jail removed before returning. Dropping the
    /// returned future cleans up as well, partly in the background.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroMetricsPoll`], any [`Error`] from chroot preparation, [`Error::Spawn`],
    /// [`Error::ExitedDuringStart`] or [`Error::ReadyTimeout`].
    pub async fn launch(config: &JailerConfig, spec: &VmSpec) -> Result<Self> {
        let paths = JailPaths::new(config, &spec.lab_id, &spec.node)?;
        if spec.timeouts.metrics_poll.is_zero() {
            return Err(Error::ZeroMetricsPoll { id: paths.id });
        }
        let span = tracing::info_span!("vm", lab = %spec.lab_id, node = %spec.node);
        async {
            let start = std::time::Instant::now();
            let mut jail = jailer::prepare_chroot(config, &paths, &spec.files).await?;
            let child = Command::new(&config.jailer)
                .args(jailer::jailer_args(
                    config,
                    &paths,
                    spec.netns.as_deref(),
                    spec.vcpu_count,
                    spec.mem_size_mib,
                ))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn();
            let child = match child {
                Ok(child) => child,
                Err(source) => {
                    if let Err(e) = jail.release().await {
                        tracing::error!(error = %e, "cleanup after failed spawn");
                    }
                    return Err(Error::Spawn {
                        id: paths.id.clone(),
                        program: config.jailer.clone(),
                        source,
                    });
                }
            };
            let mut vm = Self::start(child, jail, spec.timeouts);
            if let Err(e) = vm.wait_ready().await {
                vm.expected_exit.store(true, Ordering::SeqCst);
                vm.send_signal(Signal::SIGKILL);
                if let Err(cleanup) = vm.finish(vm.timeouts.kill).await {
                    tracing::error!(error = %cleanup, "cleanup after failed start");
                }
                return Err(e);
            }
            tracing::info!(duration_ms = start.elapsed().as_millis(), "VM API ready");
            Ok(vm)
        }
        .instrument(span)
        .await
    }

    fn start(mut child: Child, jail: Jail, timeouts: Timeouts) -> Self {
        let paths = jail.paths().clone();
        let pid = child.id();
        let mut tasks = Vec::new();
        let output = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(20),
        ));
        let (done_tx, done_rx) = mpsc::channel(1);
        // Without `--log-path`, Firecracker logs to stdout; the jailer keeps our pipes since it
        // is not daemonized.
        if let Some(out) = child.stdout.take() {
            tasks.push(tokio::spawn(
                forward_lines(out, "stdout", Arc::clone(&output), done_tx.clone())
                    .in_current_span(),
            ));
        }
        if let Some(err) = child.stderr.take() {
            tasks.push(tokio::spawn(
                forward_lines(err, "stderr", Arc::clone(&output), done_tx.clone())
                    .in_current_span(),
            ));
        }
        drop(done_tx);
        let (signals, signal_rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(ProcessState::Running);
        let expected_exit = Arc::new(AtomicBool::new(false));
        let supervisor = supervise(child, jail, signal_rx, state_tx, Arc::clone(&expected_exit));
        let supervisor = tokio::spawn(supervisor.in_current_span());
        let metrics = tail_metrics(
            paths.host_path(METRICS_FILE),
            state.clone(),
            timeouts.metrics_poll,
        );
        tasks.push(tokio::spawn(metrics.in_current_span()));
        Self {
            client: FcClient::new(paths.api_socket()),
            paths,
            timeouts,
            signals,
            state,
            expected_exit,
            supervisor: Some(supervisor),
            tasks,
            cleanup_error: None,
            pid,
            output,
            output_tasks_done: done_rx,
        }
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

    /// Resolves with the exit state; `Lost` if the supervisor is gone without publishing one.
    async fn exited(&self) -> ProcessState {
        let mut state = self.state.clone();
        state
            .wait_for(|s| *s != ProcessState::Running)
            .await
            .map_or(ProcessState::Lost, |s| s.clone())
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let client = &self.client;
        let mut state = self.state.clone();

        let paths_id = self.paths.id.clone();

        let timeouts_ready = self.timeouts.ready;
        let output_ref = &self.output;
        let done_rx = &mut self.output_tasks_done;

        let poll = async {
            while client.get::<serde_json::Value>("/").await.is_err() {
                tokio::time::sleep(READY_RETRY).await;
            }
        };
        let ready = async {
            tokio::select! {
                () = poll => Ok(()),
                res = state.wait_for(|s| *s != ProcessState::Running) => {
                    let st = res.map_or(ProcessState::Lost, |s| s.clone());
                    if tokio::time::timeout(Duration::from_millis(50), done_rx.recv()).await.is_err() {
                        tracing::debug!("output still open after startup exit");
                    }
                    let output = output_ref.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain(..).collect::<Vec<_>>().join("\n");
                    Err(Error::ExitedDuringStart {
                        id: paths_id.clone(),
                        state: st,
                        output,
                    })
                },
            }
        };
        tokio::time::timeout(timeouts_ready, ready)
            .await
            .unwrap_or_else(|_| {
                let output = self
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .drain(..)
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(Error::ReadyTimeout {
                    id: self.paths.id.clone(),
                    socket: self.paths.api_socket(),
                    timeout: self.timeouts.ready,
                    output,
                })
            })
    }

    async fn wait_exit(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.exited()).await.is_ok()
    }

    /// `SendCtrlAltDel` and the exit that should follow, both within `graceful`. Returns false
    /// if the API refuses the action (the VM is not started) or does not answer in time.
    async fn ctrl_alt_del(&self) -> bool {
        let ctrl_alt_del = InstanceActionInfo::new(ActionType::SendCtrlAltDel);
        let request = async {
            match self
                .client
                .put::<_, serde_json::Value>("/actions", &ctrl_alt_del)
                .await
            {
                Ok(_) => {
                    self.exited().await;
                    true
                }
                Err(e) => {
                    tracing::debug!(error = %e, "SendCtrlAltDel refused, skipping to SIGTERM");
                    false
                }
            }
        };
        let graceful = async {
            tokio::select! {
                stopped = request => stopped,
                _ = self.exited() => true,
            }
        };
        tokio::time::timeout(self.timeouts.graceful, graceful)
            .await
            .unwrap_or(false)
    }

    /// Stops the VM: `SendCtrlAltDel`, then SIGTERM, then SIGKILL, each after its timeout, then
    /// removes the jail and the cgroup. Returns at once if a previous call completed, so it never
    /// touches a VM launched later with the same id.
    ///
    /// # Errors
    ///
    /// [`Error::KillTimeout`] if the process survives SIGKILL (the supervisor keeps the jail and
    /// removes it once the process is reaped), [`Error::Io`] if cleanup fails, or
    /// [`Error::Task`], [`Error::CleanupTimeout`], or [`Error::CleanupFailed`] on a retry
    /// after cleanup failed.
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(message) = &self.cleanup_error {
            return Err(Error::CleanupFailed {
                id: self.paths.id.clone(),
                message: message.clone(),
            });
        }
        if self.supervisor.is_none() {
            return Ok(());
        }
        let span = tracing::info_span!("vm_shutdown", vm = %self.paths.id);
        async {
            let start = std::time::Instant::now();
            self.expected_exit.store(true, Ordering::SeqCst);
            let running = *self.state.borrow() == ProcessState::Running;
            if running && !self.ctrl_alt_del().await {
                self.send_signal(Signal::SIGTERM);
                if !self.wait_exit(self.timeouts.term).await {
                    tracing::warn!("Firecracker ignored SIGTERM, sending SIGKILL");
                    self.send_signal(Signal::SIGKILL);
                }
            }
            self.finish(self.timeouts.kill).await?;
            tracing::info!(
                duration_ms = start.elapsed().as_millis(),
                "VM stopped and cleaned up"
            );
            Ok(())
        }
        .instrument(span)
        .await
    }

    fn send_signal(&self, signal: Signal) {
        if let Err(error) = self.signals.send(signal) {
            tracing::debug!(%error, ?signal, "supervisor already stopped");
        }
    }

    /// Waits up to `timeout` for the exit, then joins the supervisor (which has released the
    /// jail by then) and the forwarding tasks.
    async fn finish(&mut self, timeout: Duration) -> Result<()> {
        if !self.wait_exit(timeout).await {
            return Err(Error::KillTimeout {
                id: self.paths.id.clone(),
                timeout,
            });
        }
        let tasks_fut = async {
            while let Some(task) = self.tasks.first_mut() {
                let result = task.await;
                self.tasks.remove(0);
                report_join(result);
            }
        };
        if tokio::time::timeout(timeout, tasks_fut).await.is_err() {
            tracing::warn!("output/metrics tasks did not finish in time, aborting them");
            for task in &self.tasks {
                task.abort();
            }
            while let Some(task) = self.tasks.first_mut() {
                let result = task.await;
                self.tasks.remove(0);
                report_join(result);
            }
        }
        tracing::debug!("post-exit tasks joined");
        let Some(supervisor) = self.supervisor.as_mut() else {
            return Ok(());
        };
        // Timeout or cancellation leaves the cleanup owner available for the next shutdown.
        let result = tokio::time::timeout(timeout, supervisor)
            .await
            .map_err(|_| Error::CleanupTimeout {
                id: self.paths.id.clone(),
                timeout,
            })?;
        self.supervisor = None;
        let result = result
            .map_err(|source| Error::Task {
                id: self.paths.id.clone(),
                task: "supervisor",
                source,
            })
            .and_then(|result| result);
        if let Err(error) = &result {
            self.cleanup_error = Some(error.to_string());
        }
        result
    }
}

fn report_join(result: std::result::Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::error!(%error, "output/metrics task failed");
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.supervisor.is_some() {
            tracing::warn!(vm = %self.paths.id, "Vm dropped without shutdown, killing it");
            self.expected_exit.store(true, Ordering::SeqCst);
        }
    }
}

/// Owns the child and its jail. It is the only place that waits on (reaps) the child, so a signal
/// sent from here can never reach a recycled pid. When the signal channel closes (the `Vm` was
/// dropped), nobody else can stop the process, so it is killed.
async fn supervise(
    mut child: Child,
    mut jail: Jail,
    mut signals: mpsc::UnboundedReceiver<Signal>,
    state: watch::Sender<ProcessState>,
    expected_exit: Arc<AtomicBool>,
) -> Result<()> {
    let pid = child
        .id()
        .and_then(|p| i32::try_from(p).ok())
        .map(Pid::from_raw);
    let res = loop {
        tokio::select! {
            res = child.wait() => break res,
            signal = signals.recv() => {
                let res = match (signal, pid) {
                    (Some(Signal::SIGKILL) | None, _) | (_, None) => child.start_kill(),
                    (Some(signal), Some(pid)) => kill(pid, signal).map_err(std::io::Error::from),
                };
                if let Err(e) = res {
                    tracing::warn!(?signal, error = %e, "failed to signal Firecracker");
                }
                if signal.is_none() {
                    break child.wait().await;
                }
            }
        }
    };
    let new_state = match res {
        Ok(status) => ProcessState::Exited(status),
        Err(e) => {
            tracing::error!(error = %e, "waiting on Firecracker failed, killing it");
            if let Err(error) = child.start_kill() {
                tracing::warn!(%error, "failed to kill Firecracker after wait failure");
            }
            ProcessState::Lost
        }
    };
    if expected_exit.load(Ordering::SeqCst) {
        tracing::info!(state = ?new_state, "Firecracker exited");
    } else {
        tracing::error!(state = ?new_state, "Firecracker exited unexpectedly");
    }
    state.send_replace(new_state);
    let res = jail.release().await;
    if let Err(e) = &res {
        tracing::error!(error = %e, "failed to remove the jail after exit");
    }
    res
}

async fn forward_lines(
    stream: impl AsyncRead + Unpin,
    source: &'static str,
    output: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    _done_tx: mpsc::Sender<()>,
) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                tracing::info!(target: "dylos::firecracker", source, "{line}");
                let mut q = output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if q.len() >= 20 {
                    q.pop_front();
                }
                q.push_back(line);
            }
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

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dylos_runtime::jailer::JailerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Name of the `#[test]` that turns the test binary into a fake jailer. The launcher re-executes
/// the test binary with libtest arguments selecting only that test; everything after `--` is
/// ignored by libtest (unmatched filters) and read by the fake.
pub const ENTRY: &str = "fake_jailer_entry";

/// What the fake does, written as the content of the `--exec-file` it receives.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// Serves the API; exits 0 on `SendCtrlAltDel`, 7 after `FlushMetrics` if `crash_on_flush`.
    Serve,
    CrashOnFlush,
    /// Answers `SendCtrlAltDel` but keeps running; dies on SIGTERM.
    IgnoreCtrlAltDel,
    /// Answers `SendCtrlAltDel` and survives SIGTERM; only SIGKILL stops it.
    IgnoreTerm,
    /// Never creates the API socket.
    NoSocket,
    /// Exits with status 3 at once.
    ExitAtOnce,
    FailDuringStart,
    /// Answers `GET /` but never answers `PUT /actions`; dies on SIGTERM.
    StallActions,
    /// Hands the first API connection to a `cat` that never answers, then exits with status 5,
    /// so the readiness request is still in flight after the exit.
    StallReadyThenExit,
    /// Spawns a background process that inherits stdout/stderr, then exits immediately.
    LeakOutputThenExit,
}

pub struct Sandbox {
    pub dir: tempfile::TempDir,
    pub config: JailerConfig,
}

impl Sandbox {
    pub fn new(mode: Mode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = std::fs::metadata(dir.path()).unwrap();
        let exec = dir.path().join("firecracker");
        std::fs::write(&exec, format!("{mode:?}")).unwrap();
        let mut config = JailerConfig::new(dir.path().join("jails"), meta.uid(), meta.gid());
        config.cgroup_root = dir.path().join("cgroup");
        config.jailer = std::env::current_exe().unwrap();
        config.firecracker = exec;
        config.launcher_args = [ENTRY, "--exact", "--nocapture", "--"]
            .into_iter()
            .map(OsString::from)
            .collect();
        Self { dir, config }
    }

    pub fn assert_no_jail_left(&self) {
        let base = self.dir.path().join("jails/firecracker");
        let left: Vec<PathBuf> = match std::fs::read_dir(base) {
            Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        };
        assert_eq!(left, Vec::<PathBuf>::new());
        let cgroups = self.dir.path().join("cgroup/firecracker");
        let left: Vec<PathBuf> = match std::fs::read_dir(cgroups) {
            Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        };
        assert_eq!(left, Vec::<PathBuf>::new(), "cgroups left behind");
    }

    pub async fn release_output_holder(&self) {
        let socket = self.dir.path().join("output-holder.sock");
        let stream = UnixStream::connect(socket).await.unwrap();
        drop(stream);
        let pid = std::fs::read_to_string(self.dir.path().join("output-holder.pid")).unwrap();
        wait_until("output holder is reaped", Duration::from_secs(5), || {
            !Path::new(&format!("/proc/{pid}")).exists()
        })
        .await;
    }

    pub fn jail_dir(&self, id: &str) -> PathBuf {
        self.dir.path().join("jails/firecracker").join(id)
    }
}

/// Polls `condition` until it holds; panics after `timeout`.
pub async fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let res = tokio::time::timeout(timeout, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(res.is_ok(), "timed out waiting until {what}");
}

/// Runs the fake jailer if this process was launched as one; returns otherwise.
pub fn run_fake_jailer_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    let Some(sep) = args.iter().position(|a| a == "--") else {
        return;
    };
    let args = &args[sep + 1..];
    let flag = |name: &str| {
        let i = args.iter().position(|a| a == name)?;
        args.get(i + 1).cloned()
    };
    let (Some(base), Some(exec), Some(id), Some(sock), Some(metrics)) = (
        flag("--chroot-base-dir"),
        flag("--exec-file"),
        flag("--id"),
        flag("--api-sock"),
        flag("--metrics-path"),
    ) else {
        return;
    };
    let root = Path::new(&base).join("firecracker").join(&id).join("root");
    // Like the real jailer, which creates `<cgroup root>/<parent>/<id>` when given a `--cgroup`.
    // The sandbox mounts its fake cgroup root next to the chroot base.
    if args.iter().any(|a| a == "--cgroup") {
        let parent = flag("--parent-cgroup").unwrap_or_else(|| "firecracker".into());
        let cgroup = Path::new(&base).parent().unwrap().join("cgroup");
        std::fs::create_dir_all(cgroup.join(parent).join(&id)).unwrap();
    }
    let mode = std::fs::read_to_string(&exec).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if mode == "HoldOutput" {
        rt.block_on(async {
            let listener = UnixListener::bind(Path::new(&base).join("output-holder.sock")).unwrap();
            let _connection = listener.accept().await.unwrap();
        });
        std::process::exit(0);
    }
    let properties: Vec<_> = args
        .windows(2)
        .filter(|w| w[0] == "--cgroup")
        .map(|w| w[1].as_str())
        .collect();
    std::fs::write(root.join("cgroup-args"), properties.join("\n")).unwrap();
    let sandbox = Path::new(&base).parent().unwrap().to_path_buf();
    let code = rt.block_on(fake_firecracker(
        &mode,
        &sandbox,
        root.join(sock.trim_start_matches('/')),
        root.join(metrics.trim_start_matches('/')),
    ));
    std::process::exit(code);
}

async fn fake_firecracker(mode: &str, sandbox: &Path, sock: PathBuf, metrics: PathBuf) -> i32 {
    println!("fake firecracker starting in mode {mode}");
    match mode {
        "ExitAtOnce" => return 3,
        "FailDuringStart" => {
            eprintln!("fake jailer failed during start");
            return 1;
        }
        "NoSocket" => std::future::pending::<()>().await,
        _ => {}
    }
    let _term = if mode == "IgnoreTerm" {
        Some(tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap())
    } else {
        None
    };
    let listener = UnixListener::bind(&sock).unwrap();
    let mut stalled = Vec::new();
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        if mode == "StallReadyThenExit" {
            // A blocking fd: `cat` would fail on EAGAIN and close the connection.
            let stream = stream.into_std().unwrap();
            stream.set_nonblocking(false).unwrap();
            // Backgrounded by `sh` so that it outlives this process; an asynchronous list gets
            // /dev/null as stdin unless redirected, hence fd 3.
            let status = std::process::Command::new("sh")
                .args(["-c", "exec 3<&0; cat <&3 >/dev/null 2>&1 &"])
                .stdin(std::os::fd::OwnedFd::from(stream))
                .status()
                .unwrap();
            assert!(status.success());
            return 5;
        }
        let request = read_request(&mut stream).await;
        if mode == "StallActions" && request.starts_with("PUT /actions ") {
            stalled.push(stream);
            continue;
        }
        let reply = if request.starts_with("GET / ") {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"
        } else {
            "HTTP/1.1 204 No Content\r\n\r\n"
        };
        stream.write_all(reply.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        if request.contains("SendCtrlAltDel") && mode == "LeakOutputThenExit" {
            hold_stderr(sandbox).await;
            return 0;
        }
        if request.contains("SendCtrlAltDel") && mode == "Serve" {
            return 0;
        }
        if request.contains("FlushMetrics") {
            let mut line =
                r#"{"utc_timestamp_ms":1,"api_server":{"process_startup_time_us":42}}"#.to_owned();
            line.push('\n');
            let mut f = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&metrics)
                .await
                .unwrap();
            f.write_all(line.as_bytes()).await.unwrap();
            // Tokio buffers writes; process::exit must not terminate the pending blocking write.
            f.flush().await.unwrap();
            if mode == "CrashOnFlush" {
                return 7;
            }
        }
    }
}

/// Simulates a Firecracker that exits while a process it spawned still holds its stderr: the
/// launcher's stderr pipe then never reaches EOF, which is what `Vm::finish` must survive with a
/// bounded wait. The holder is this test binary again, in `HoldOutput` mode; it inherits stderr,
/// binds `<sandbox>/output-holder.sock` to signal it is running, and stays alive until the test
/// connects to that socket and closes it (`Sandbox::release_output_holder`). `sandbox` is the
/// parent of the chroot base directory.
async fn hold_stderr(sandbox: &Path) {
    let base = sandbox;
    let holder = base.join("holder-mode");
    std::fs::write(&holder, "HoldOutput").unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([ENTRY, "--exact", "--nocapture", "--", "--chroot-base-dir"])
        .arg(base)
        .arg("--exec-file")
        .arg(holder)
        .args([
            "--id",
            "holder",
            "--api-sock",
            "/unused",
            "--metrics-path",
            "/unused",
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(base.join("output-holder.pid"), child.id().to_string()).unwrap();
    wait_until("output holder socket", Duration::from_secs(5), || {
        base.join("output-holder.sock").exists()
    })
    .await;
    // The holder retains only stderr, so stdout completes before shutdown times out.
    drop(child);
}

async fn read_request(stream: &mut UnixStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len || n == 0 {
                return text;
            }
        } else if n == 0 {
            return text;
        }
    }
}

/// Records the message of every `tracing` event emitted on this thread while it is alive.
pub struct Captured(pub Arc<Mutex<Vec<String>>>);

impl Captured {
    pub fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            Recorder(Arc::clone(&lines)),
        );
        (Self(lines), tracing::subscriber::set_default(subscriber))
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(needle))
    }
}

struct Recorder(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Visit(String);
        impl tracing::field::Visit for Visit {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = write!(self.0, "{}={value:?} ", field.name());
            }
        }
        let mut visit = Visit(format!("{} ", event.metadata().target()));
        event.record(&mut visit);
        self.0.lock().unwrap().push(visit.0);
    }
}

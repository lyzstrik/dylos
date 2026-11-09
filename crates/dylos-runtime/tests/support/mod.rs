#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dylos_runtime::Vm;
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
    ParallelPause,
    Echo,
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
    EarlyReadiness,
    SpamOutput,
    OversizedOutput,
    FalseReadiness,
    LabReady,
    LabPartialLaunch,
    LabConfigureFailure,
    LabNetworkFailure,
    LabCrashOnFlush,
    /// `LabReady` without its configuration barriers, with the `ParallelPause` and snapshot fakes.
    LabSnapshot,
}

/// Returns a short temporary directory suitable for test jails and stores.
///
/// Reflinks (used for microVM images and snapshots) require the image,
/// lab store, and jail directory to reside on the same filesystem mount.
/// On systems where `/tmp` is on a different mount from the workspace
/// (such as GitHub Actions runners with a loop-mounted XFS filesystem or
/// hosts with a tmpfs `/tmp`), `/tmp` cannot be used for reflinks.
///
/// To keep Unix domain socket paths comfortably below Linux's 108-byte `sockaddr_un`
/// limit even on deep checkout paths, this helper selects a short, operator-controlled
/// location on the required filesystem independently of checkout depth:
/// 1. `DYLOS_TEST_TMPDIR` if set by the operator.
/// 2. `/tmp` if on the same mount as the workspace.
/// 3. `/var/tmp` if on the same mount as the workspace.
/// 4. An operator-controlled `.t` directory under `$HOME` if on the same mount.
/// 5. A `.t` directory at the mount root or the shallowest writable ancestor
///    on the same mount (never descending into deep checkout trees).
pub fn test_tempdir() -> tempfile::TempDir {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    test_tempdir_for(&workspace)
}

pub fn test_tempdir_for(workspace: &Path) -> tempfile::TempDir {
    test_tempdir_with_tmp(workspace, Path::new("/tmp"))
}

pub fn test_tempdir_with_tmp(workspace: &Path, tmp_dir: &Path) -> tempfile::TempDir {
    if let Some(custom) = std::env::var_os("DYLOS_TEST_TMPDIR").filter(|s| !s.is_empty()) {
        let base = PathBuf::from(custom);
        std::fs::create_dir_all(&base).unwrap();
        return tempfile::Builder::new()
            .prefix("")
            .tempdir_in(&base)
            .unwrap();
    }

    let base = select_test_temp_base(workspace, tmp_dir);
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("")
        .tempdir_in(&base)
        .unwrap()
}

fn is_writable_dir(path: &Path) -> bool {
    path.is_dir()
        && tempfile::Builder::new()
            .prefix(".probe")
            .tempfile_in(path)
            .is_ok()
}

fn try_temp_base(path: &Path) -> bool {
    std::fs::create_dir_all(path).is_ok() && is_writable_dir(path)
}

pub fn select_test_temp_base(workspace: &Path, tmp_dir: &Path) -> PathBuf {
    let canonical_ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let ws_dev = match std::fs::metadata(&canonical_ws) {
        Ok(m) => m.dev(),
        Err(_) => return tmp_dir.to_path_buf(),
    };

    // 1. Probe candidate standard temporary locations
    if std::fs::metadata(tmp_dir).is_ok_and(|meta| meta.dev() == ws_dev && is_writable_dir(tmp_dir))
    {
        return tmp_dir.to_path_buf();
    }

    let var_tmp = Path::new("/var/tmp");
    if std::fs::metadata(var_tmp).is_ok_and(|meta| meta.dev() == ws_dev && is_writable_dir(var_tmp))
    {
        return var_tmp.to_path_buf();
    }

    // 2. Probe operator's home directory if on the same mount
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && std::fs::metadata(&home).is_ok_and(|meta| meta.dev() == ws_dev)
    {
        let candidate = home.join(".t");
        if try_temp_base(&candidate) {
            return candidate;
        }
    }

    // 3. Find the mount root of the required filesystem
    let mut mount_root = canonical_ws.clone();
    while let Some(parent) = mount_root.parent() {
        if std::fs::metadata(parent).is_ok_and(|meta| meta.dev() == ws_dev) {
            mount_root = parent.to_path_buf();
            continue;
        }
        break;
    }

    let mount_t = mount_root.join(".t");
    if try_temp_base(&mount_t) {
        return mount_t;
    }

    // 4. Walk down from mount_root along ancestors towards canonical_ws,
    // selecting the shallowest writable ancestor
    if let Ok(rel) = canonical_ws.strip_prefix(&mount_root) {
        let mut current = mount_root;
        for component in rel.components() {
            current.push(component);
            let candidate = current.join(".t");
            if try_temp_base(&candidate) {
                return candidate;
            }
        }
    }

    // Fallback if none of the ancestors were writable
    canonical_ws.join(".t")
}

const STATIC_BINARY_FIXTURE: &[u8] = include_bytes!("../fixtures/elf/static_binary");

pub struct Sandbox {
    pub dir: tempfile::TempDir,
    pub config: JailerConfig,
}

impl Sandbox {
    pub fn new(mode: Mode) -> Self {
        Self::with_dir(mode, test_tempdir())
    }

    pub fn in_target(mode: Mode) -> Self {
        Self::new(mode)
    }

    fn with_dir(mode: Mode, dir: tempfile::TempDir) -> Self {
        let meta = std::fs::metadata(dir.path()).unwrap();
        let exec = dir.path().join("firecracker");
        let mut content = STATIC_BINARY_FIXTURE.to_vec();
        content.extend_from_slice(format!("{mode:?}").as_bytes());
        std::fs::write(&exec, content).unwrap();
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

    /// Makes every jail started from now on hold a mount that blocks its removal.
    pub fn make_teardown_fail(&self) {
        std::fs::write(self.dir.path().join("busy-jail"), b"").unwrap();
    }

    /// Releases the mounts of `make_teardown_fail` so the leftovers can be cleaned up.
    pub fn release_busy_jails(&self) {
        let base = self.dir.path().join("jails/firecracker");
        for entry in std::fs::read_dir(base).into_iter().flatten() {
            let busy = entry.unwrap().path().join("root/busy");
            if busy.exists() {
                nix::mount::umount2(&busy, nix::mount::MntFlags::MNT_DETACH).unwrap();
            }
        }
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(res.is_ok(), "timed out waiting until {what}");
}

/// Like `wait_until`, for a condition that has to await.
pub async fn wait_until_async<F: Future<Output = bool>>(
    what: &str,
    timeout: Duration,
    mut condition: impl FnMut() -> F,
) {
    let res = tokio::time::timeout(timeout, async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
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
    let raw = std::fs::read(&exec).unwrap();
    let mode = if raw.starts_with(STATIC_BINARY_FIXTURE) {
        std::str::from_utf8(&raw[STATIC_BINARY_FIXTURE.len()..])
            .unwrap()
            .to_string()
    } else {
        String::from_utf8(raw).unwrap()
    };
    // A mount point inside the jail makes the lab's teardown fail with EBUSY.
    if Path::new(&base)
        .parent()
        .unwrap()
        .join("busy-jail")
        .exists()
    {
        let busy = root.join("busy");
        std::fs::create_dir_all(&busy).unwrap();
        nix::mount::mount(
            Some("tmpfs"),
            &busy,
            Some("tmpfs"),
            nix::mount::MsFlags::empty(),
            None::<&str>,
        )
        .unwrap();
    }
    if mode == "LabPartialLaunch" && id.ends_with("-B") {
        std::process::exit(3);
    }
    if mode.starts_with("Lab") {
        std::fs::write(root.join("netns-arg"), flag("--netns").unwrap_or_default()).unwrap();
    }
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

async fn parallel_pause_reply(
    mode: &str,
    sandbox: &Path,
    sock: &Path,
    request: &str,
    stream: &mut UnixStream,
) -> bool {
    if matches!(mode, "ParallelPause" | "LabSnapshot") && request.starts_with("PATCH /vm ") {
        let root = sock.parent().unwrap().parent().unwrap();
        let action = if request.contains("Paused") {
            "paused"
        } else {
            "resumed"
        };
        let id = root
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        std::fs::write(sandbox.join(format!("{id}.{action}")), b"confirmed request").unwrap();
        // No VM answers until all three requests arrived: serial dispatch fails deterministically.
        wait_until("all VM state requests", Duration::from_secs(3), || {
            std::fs::read_dir(sandbox)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == action)
                })
                .count()
                == 3
        })
        .await;
        std::fs::write(sandbox.join(format!("{id}.{action}-barrier")), b"").unwrap();
        if action == "paused" {
            std::fs::write(root.join("is-paused"), b"").unwrap();
            assert_taps_down(
                sandbox,
                "barrier violation: fabric must be frozen before VMs are paused, but TAP is UP",
            );
            if root.join("stall-pause").exists() {
                std::fs::write(root.join("pause-started"), b"").unwrap();
                wait_until("pause stall released", Duration::from_secs(5), || {
                    !root.join("stall-pause").exists()
                })
                .await;
            }
            if root.join("drop-paused").exists() {
                stream.shutdown().await.unwrap();
                return true;
            }
        }
        if root.join(format!("fail-{action}")).exists() {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            return true;
        }
        if action == "resumed" {
            if root.join("is-paused").exists() {
                std::fs::remove_file(root.join("is-paused")).unwrap();
            }
            let jail = root.parent().unwrap();
            let parent = jail.parent().unwrap();
            let own_id = jail.file_name().unwrap().to_str().unwrap();
            if let Some((prefix, _)) = own_id.rsplit_once('-') {
                let any_captured = ["A", "B", "C"].iter().any(|node| {
                    let r = parent.join(format!("{prefix}-{node}/root"));
                    r.join("capture-done").exists() || r.join("snapshot-started").exists()
                });
                if any_captured {
                    for node in ["A", "B", "C"] {
                        let r = parent.join(format!("{prefix}-{node}/root"));
                        assert!(
                            r.join("capture-done").exists(),
                            "barrier violation: VM resumed before capture finished for {node}"
                        );
                    }
                }
            }
            if root.join("stall-resume").exists() {
                std::fs::write(root.join("resume-started"), b"").unwrap();
                wait_until("resume stall released", Duration::from_secs(5), || {
                    !root.join("stall-resume").exists()
                })
                .await;
            }
            assert_taps_down(
                sandbox,
                "barrier violation: fabric must remain frozen until all VMs resumed, but TAP is UP",
            );
        }
    }
    false
}

pub fn assert_taps_down(sandbox: &Path, barrier_msg: &str) {
    let netns_dir = sandbox.join("netns");
    if !netns_dir.exists() {
        return;
    }
    let entries = std::fs::read_dir(&netns_dir)
        .expect("failed to read netns directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("failed to read netns directory entry");
    assert!(
        !entries.is_empty(),
        "netns directory exists but contains no netns entries"
    );
    for entry in entries {
        let netns_path = entry.path();
        let output = std::process::Command::new("nsenter")
            .args([
                format!("--net={}", netns_path.display()).as_str(),
                "ip",
                "link",
                "show",
            ])
            .output()
            .expect("failed to run nsenter ip link show");
        assert!(
            output.status.success(),
            "nsenter ip link show failed on {}: status {:?}, stderr: {}",
            netns_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let link_out = String::from_utf8_lossy(&output.stdout);
        for tap in ["tap-A-eth0", "tap-B-eth0", "tap-C-eth0"] {
            let mut found = false;
            for line in link_out.lines() {
                if line.contains(tap) {
                    found = true;
                    assert!(
                        !line.contains(",UP") && !line.contains("<UP,"),
                        "{barrier_msg}: {line}"
                    );
                }
            }
            assert!(
                found,
                "missing expected TAP interface {tap} in netns {}: output:\n{link_out}",
                netns_path.display()
            );
        }
    }
}

async fn check_lab_request(
    mode: &str,
    sandbox: &Path,
    sock: &Path,
    request: &str,
    stream: &mut UnixStream,
) -> bool {
    record_lab_request(mode, sock, request);
    assert_lab_barriers(mode, sock, request);
    parallel_pause_reply(mode, sandbox, sock, request, stream).await
}

#[allow(clippy::too_many_lines)]
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
    if mode == "EarlyReadiness" || mode == "Echo" {
        println!("dylos: ready");
        if mode == "EarlyReadiness" {
            println!("early marker forwarded");
        }
    }
    if mode == "Echo" || mode.starts_with("Lab") {
        spawn_echo_responder(mode, sandbox);
    }
    let mut stalled = Vec::new();
    let mut oversized_sent = false;
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
        let request = normalize_lab_request(mode, read_request(&mut stream).await);
        if check_lab_request(mode, sandbox, &sock, &request, &mut stream).await {
            continue;
        }
        if request.starts_with("PUT /snapshot/load ") {
            let root = sock.parent().unwrap().parent().unwrap();
            assert!(request.contains("\"backend_type\":\"File\""));
            assert!(request.contains("\"resume_vm\":false"));
            let netns = std::fs::read_to_string(root.join("netns-arg")).unwrap();
            let output = std::process::Command::new("nsenter")
                .arg(format!("--net={netns}"))
                .args(["ip", "-j", "link", "show"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let links: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            for link in links
                .as_array()
                .unwrap()
                .iter()
                .filter(|l| l["ifname"] != "lo")
            {
                assert!(
                    !link["flags"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|flag| flag == "UP"),
                    "fabric was up during load: {link}"
                );
            }
            std::fs::write(root.join("load-done"), b"").unwrap();
        }
        if request.starts_with("PATCH /vm ") && request.contains("Resumed") {
            let root = sock.parent().unwrap().parent().unwrap();
            if root.join("load-done").exists() {
                let agent_socket = root.join("vsock.sock_5000");
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let stream = tokio::net::UnixStream::connect(agent_socket).await.unwrap();
                    let mut reader = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    assert!(matches!(
                        dylos_agent::protocol::Request::from_line(&line)
                            .unwrap()
                            .body,
                        dylos_agent::protocol::RequestBody::InjectEntropy { .. }
                    ));
                    reader
                        .get_mut()
                        .write_all(b"{\"v\":1,\"type\":\"ok\"}\n")
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let request = dylos_agent::protocol::Request::from_line(&line).unwrap();
                    assert!(matches!(
                        request.body,
                        dylos_agent::protocol::RequestBody::Resync { .. }
                    ));
                    reader
                        .get_mut()
                        .write_all(b"{\"v\":1,\"type\":\"ok\"}\n")
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    assert_eq!(
                        dylos_agent::protocol::Request::from_line(&line)
                            .unwrap()
                            .body,
                        dylos_agent::protocol::RequestBody::Health
                    );
                    let reply = dylos_agent::protocol::Reply::new(
                        dylos_agent::protocol::ReplyBody::Health {
                            uptime_ms: 0,
                            wall_clock_unix_ns: 0,
                            agent_version: "fake".into(),
                        },
                    );
                    reader
                        .get_mut()
                        .write_all(reply.to_line().unwrap().as_bytes())
                        .await
                        .unwrap();
                });
                let jail = root.parent().unwrap();
                let prefix = jail
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .rsplit_once('-')
                    .unwrap()
                    .0;
                for node in ["A", "B", "C"] {
                    assert!(
                        jail.parent()
                            .unwrap()
                            .join(format!("{prefix}-{node}/root/load-done"))
                            .exists(),
                        "resume before every load"
                    );
                }
            }
        }
        if let Ok(stage) = std::fs::read_to_string(sandbox.join("fail-restore")) {
            let fail = stage == "load" && request.starts_with("PUT /snapshot/load ")
                || stage == "resume"
                    && request.starts_with("PATCH /vm ")
                    && request.contains("Resumed");
            if fail {
                stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
                continue;
            }
        }
        if request.contains("InstanceStart") {
            if matches!(
                mode,
                "LabReady" | "LabPartialLaunch" | "LabCrashOnFlush" | "LabSnapshot"
            ) {
                println!("dylos: ready");
            }
            if mode == "LabNetworkFailure" {
                println!("dylos: network setup failed");
            }
            if mode == "SpamOutput" {
                for i in 0..4096 {
                    println!("before {i}");
                }
                println!("dylos: ready");
                for i in 0..4096 {
                    println!("after {i}");
                }
                println!("burst forwarded");
            } else if mode == "FalseReadiness" {
                println!("this is just a line with dylos: ready inside it");
                println!("dylos: network setup failed");
                println!("dylos: ready");
            }
        }
        if mode == "StallActions" && request.starts_with("PUT /actions ") {
            stalled.push(stream);
            continue;
        }
        if request.starts_with("PUT /snapshot/create ") {
            let root = sock.parent().unwrap().parent().unwrap();
            std::fs::write(root.join("snapshot-request"), &request).unwrap();
            if root.join("stall-snapshot").exists() {
                std::fs::write(root.join("snapshot-started"), b"").unwrap();
                wait_until("snapshot stall released", Duration::from_secs(5), || {
                    !root.join("stall-snapshot").exists()
                })
                .await;
            }
            if root.join("fail-snapshot").exists() {
                stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
                continue;
            }
            std::fs::write(root.join("snap.state"), b"fake vm state content").unwrap();
            std::fs::write(root.join("snap.mem"), b"fake vm memory content").unwrap();
            std::fs::write(root.join("capture-done"), b"").unwrap();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
            continue;
        }
        let reply = if mode == "LabConfigureFailure" && request.starts_with("PUT /boot-source ") {
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\n\r\n{}"
        } else if request.starts_with("GET / ") {
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
        if request.contains("SendCtrlAltDel")
            && (matches!(mode, "Serve" | "ParallelPause" | "Echo" | "OversizedOutput")
                || mode.starts_with("Lab"))
        {
            return 0;
        }
        if request.contains("FlushMetrics") {
            if mode == "OversizedOutput" {
                use std::io::Write;
                for mut stream in [
                    Box::new(std::io::stdout()) as Box<dyn std::io::Write>,
                    Box::new(std::io::stderr()),
                ] {
                    if oversized_sent {
                        stream.write_all(b"\nconsole-recovered\n").unwrap();
                    } else {
                        stream.write_all(&vec![b'x'; 256 * 1024]).unwrap();
                    }
                    stream.flush().unwrap();
                }
                oversized_sent = true;
            }
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
            if mode == "CrashOnFlush" || mode == "LabCrashOnFlush" {
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

    pub fn contains_fields(&self, message: &str, fields: &[&str]) -> bool {
        self.0
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(message) && fields.iter().all(|field| line.contains(field)))
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l.contains(needle))
    }
}

struct Recorder(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visit(String);
        impl tracing::field::Visit for Visit {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                write!(self.0, "{}={value} ", field.name()).unwrap();
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                write!(self.0, "{}={value:?} ", field.name()).unwrap();
            }
        }
        let mut visit = Visit(format!("span={} ", attrs.metadata().name()));
        attrs.record(&mut visit);
        self.0.lock().unwrap().push(visit.0);
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Visit(String);
        impl tracing::field::Visit for Visit {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                write!(self.0, "{}={value:?} ", field.name()).unwrap();
            }
        }
        let mut visit = Visit(format!("{} ", event.metadata().target()));
        event.record(&mut visit);
        self.0.lock().unwrap().push(visit.0);
    }
}

pub async fn readiness_case(
    sandbox: &Sandbox,
    spec: &dylos_runtime::VmSpec,
    mode: Mode,
    forwarded: Option<&str>,
) -> dylos_runtime::Result<Duration> {
    let mut vm = Vm::launch(&sandbox.config, spec).await.unwrap();
    let pid = vm.pid().unwrap();
    let paths = vm.paths().clone();
    let (logs, _guard) = Captured::install();
    let start = if matches!(mode, Mode::EarlyReadiness) {
        Ok(None)
    } else {
        vm.client()
            .put::<_, serde_json::Value>(
                "/actions",
                &dylos_fc::config::InstanceActionInfo::instance_start(),
            )
            .await
    };
    let result = async {
        if let Some(line) = forwarded {
            let timeout = Duration::from_secs(5);
            tokio::time::timeout(timeout, async {
                while !vm.output().iter().any(|entry| entry == line) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| dylos_runtime::Error::ReadinessTimeout {
                id: paths.id.clone(),
                timeout,
            })?;
        }
        vm.wait_for_ready(Duration::from_millis(100)).await
    }
    .await;
    let shutdown = vm.shutdown().await;
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    assert!(!paths.jail_dir.exists());
    assert!(!paths.cgroup_dir.exists());
    assert!(!paths.api_socket().exists());
    sandbox.assert_no_jail_left();
    shutdown.unwrap();
    start.unwrap();
    if let Ok(duration) = result {
        assert!(logs.contains("VM boot ready"));
        assert!(logs.contains(&format!("duration_ms={}", duration.as_millis())));
        assert_eq!(
            vm.wait_for_ready(Duration::from_secs(1)).await.unwrap(),
            duration
        );
    }
    result
}

fn record_lab_request(mode: &str, sock: &Path, request: &str) {
    if mode.starts_with("Lab") {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(sock.parent().unwrap().parent().unwrap().join("requests"))
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
    }
}

fn assert_lab_barriers(mode: &str, sock: &Path, request: &str) {
    if mode != "LabReady" {
        return;
    }
    let jail = sock.parent().unwrap().parent().unwrap().parent().unwrap();
    let parent = jail.parent().unwrap();
    let own_id = jail.file_name().unwrap().to_str().unwrap();
    let prefix = own_id.rsplit_once('-').unwrap().0;
    for node in ["A", "B", "C"] {
        let root = parent.join(format!("{prefix}-{node}/root"));
        if request.starts_with("PUT /machine-config ") {
            assert!(
                root.join("run/firecracker.socket").exists(),
                "configuration before all VMs launched"
            );
        }
        if request.contains("InstanceStart") {
            let requests = std::fs::read_to_string(root.join("requests")).unwrap();
            assert!(requests.contains("PUT /drives/rootfs "));
            let interface = if node == "B" { "eth1" } else { "eth0" };
            assert!(
                requests.contains(&format!("PUT /network-interfaces/{interface} ")),
                "start before all VMs configured"
            );
        }
    }
}

fn normalize_lab_request(mode: &str, request: String) -> String {
    if mode.starts_with("Lab") || mode == "ParallelPause" {
        request.replacen(" http://localhost/", " /", 1)
    } else {
        request
    }
}

/// Superblock-wide summary counters that a read-only `e2fsck -fn` reported as stale.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StaleSummaryCounters {
    pub free_blocks: bool,
    pub free_inodes: bool,
}

const E2FSCK_PASSES: [&str; 5] = [
    "Pass 1: Checking inodes, blocks, and sizes",
    "Pass 2: Checking directory structure",
    "Pass 3: Checking directory connectivity",
    "Pass 4: Checking reference counts",
    "Pass 5: Checking group summary information",
];

/// Verifies the output of a read-only `e2fsck -fn` on a recovered disk image.
///
/// ext4 keeps free block and inode counts in per-CPU counters and writes the superblock-wide
/// totals (`s_free_blocks_count`, `s_free_inodes_count`) only on a clean unmount or a
/// superblock commit; group descriptors and bitmaps are journaled. An image of a mounted,
/// writing filesystem (a crash or a crash-consistent snapshot) can therefore carry stale
/// superblock totals after journal replay. Those two pass-5 diagnostics, each followed by the
/// `-n` answer `Fix? no`, are the only ones accepted; any group-level, bitmap or structural
/// diagnostic is rejected.
///
/// The transcript must also be complete: status 0, all five passes once and in order, and one
/// final `<device>: N/N files (P% non-contiguous), N/N blocks` summary after pass 5.
pub fn verify_e2fsck_readonly_output(
    output: &str,
    exit_code: Option<i32>,
) -> Result<StaleSummaryCounters, String> {
    if exit_code != Some(0) {
        return Err(format!(
            "e2fsck exited with non-zero status: {exit_code:?}\nOutput:\n{output}"
        ));
    }
    let mut stale = StaleSummaryCounters::default();
    let mut passes = 0;
    let mut summary = false;
    let mut banner = false;
    let mut library = false;
    let mut lines = output
        .lines()
        .map(str::trim)
        .enumerate()
        .filter(|(_, line)| !line.is_empty());
    while let Some((index, line)) = lines.next() {
        let lineno = index + 1;
        let reject = |why: &str| Err(format!("line {lineno}: {why}: {line:?}"));
        if passes < E2FSCK_PASSES.len() && line == E2FSCK_PASSES[passes] {
            if summary {
                return reject("e2fsck pass after the final summary");
            }
            passes += 1;
        } else if !banner && is_e2fsck_banner(line) {
            banner = true;
        } else if !library && line.starts_with("Using EXT2FS Library version ") {
            library = true;
        } else if passes == E2FSCK_PASSES.len() && !summary && is_e2fsck_summary(line) {
            summary = true;
        } else if let Some(counter) = stale_summary_counter(line) {
            if passes != E2FSCK_PASSES.len() || summary {
                return reject("superblock counter mismatch outside pass 5");
            }
            let seen = match counter {
                SummaryCounter::FreeBlocks => &mut stale.free_blocks,
                SummaryCounter::FreeInodes => &mut stale.free_inodes,
            };
            if std::mem::replace(seen, true) {
                return reject("repeated superblock counter mismatch");
            }
            match lines.next() {
                Some((_, prompt)) if is_fix_no_prompt(prompt) => {}
                next => {
                    return Err(format!(
                        "line {lineno}: expected 'Fix? no' after allowlisted superblock counter mismatch, got: {:?}",
                        next.map(|(_, l)| l)
                    ));
                }
            }
        } else {
            return reject("unallowlisted e2fsck diagnostic or error");
        }
    }
    if passes != E2FSCK_PASSES.len() {
        return Err(format!(
            "incomplete e2fsck transcript: missing {:?}\nOutput:\n{output}",
            E2FSCK_PASSES[passes]
        ));
    }
    if !summary {
        return Err(format!(
            "incomplete e2fsck transcript: missing final summary\nOutput:\n{output}"
        ));
    }
    Ok(stale)
}

enum SummaryCounter {
    FreeBlocks,
    FreeInodes,
}

/// Only the superblock-wide form: the group form reads `... wrong for group #N (...)`.
fn stale_summary_counter(line: &str) -> Option<SummaryCounter> {
    let (counter, rest) = if let Some(rest) = line.strip_prefix("Free blocks count wrong (") {
        (SummaryCounter::FreeBlocks, rest)
    } else {
        (
            SummaryCounter::FreeInodes,
            line.strip_prefix("Free inodes count wrong (")?,
        )
    };
    let (recorded, actual) = rest.strip_suffix(").")?.split_once(", counted=")?;
    (is_number(recorded) && is_number(actual)).then_some(counter)
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `e2fsck 1.47.0 (5-Feb-2023)`
fn is_e2fsck_banner(line: &str) -> bool {
    line.strip_prefix("e2fsck ")
        .and_then(|rest| rest.split_once(" ("))
        .is_some_and(|(version, date)| {
            !version.is_empty()
                && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && date.ends_with(')')
                && !date[..date.len() - 1].contains(['(', ')'])
        })
}

/// `<device>: 885/32768 files (1.7% non-contiguous), 28698/131072 blocks`; a forced check
/// never prints the `clean,` form.
fn is_e2fsck_summary(line: &str) -> bool {
    let Some((device, rest)) = line.rsplit_once(": ") else {
        return false;
    };
    let ratio = |text: &str| {
        text.split_once('/')
            .is_some_and(|(a, b)| is_number(a) && is_number(b))
    };
    let Some((files, rest)) = rest.split_once(" files (") else {
        return false;
    };
    let Some((fragmented, blocks)) = rest.split_once("% non-contiguous), ") else {
        return false;
    };
    !device.is_empty()
        && ratio(files)
        && fragmented
            .split_once('.')
            .map_or(is_number(fragmented), |(a, b)| is_number(a) && is_number(b))
        && blocks.strip_suffix(" blocks").is_some_and(ratio)
}

fn is_fix_no_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed == "Fix? no" {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix("Fix") {
        if let Some(after_q) = rest.strip_prefix("? no") {
            return after_q.is_empty();
        }
        if let Some(bracket_end) = rest.find(">?") {
            let num = &rest[1..bracket_end];
            let after_q = rest[bracket_end + 2..].trim_start();
            return rest.starts_with('<')
                && !num.is_empty()
                && num.chars().all(|c| c.is_ascii_digit())
                && after_q == "no";
        }
    }
    false
}

fn spawn_echo_responder(mode: &str, sandbox: &Path) {
    let mode = mode.to_string();
    let sandbox = sandbox.to_path_buf();
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let _ = std::fs::write(sandbox.join("exec-lock-acquired"), b"1");
            if sandbox.join("exec-gate").exists() {
                let _ = std::fs::remove_file(sandbox.join("exec-gate"));
                let release = sandbox.join("exec-release");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !release.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let _ = std::fs::remove_file(&release);
            }
            let cmd = if mode.starts_with("Lab") && line.contains("ping ") {
                line.replace("ping ", ": ")
            } else {
                line
            };
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output();
            if let Ok(out) = output {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&out.stdout);
                let _ = stdout.write_all(&out.stderr);
                let _ = stdout.flush();
            }
        }
    });
}

pub struct E2ePrerequisites {
    pub workspace: PathBuf,
    pub bin: PathBuf,
    pub kernel: PathBuf,
    pub image: PathBuf,
}

pub fn check_e2e_prerequisites(test: &str) -> Option<E2ePrerequisites> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED {test}: DYLOS_FC_BIN_DIR is not set");
        return None;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("SKIPPED {test}: must start as root");
        return None;
    }
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");
    for path in [
        Path::new("/dev/kvm"),
        &bin.join("jailer"),
        &bin.join("firecracker"),
        &kernel,
        &image,
    ] {
        if !path.exists() {
            eprintln!("SKIPPED {test}: {} is missing", path.display());
            return None;
        }
    }
    Some(E2ePrerequisites {
        workspace,
        bin,
        kernel,
        image,
    })
}

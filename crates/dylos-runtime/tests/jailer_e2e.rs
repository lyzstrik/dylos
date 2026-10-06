#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use dylos_fc::config::{BootSource, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailerConfig};
use dylos_runtime::{Timeouts, Vm, VmSpec};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const NOBODY: u32 = 65534;

/// The upstream static binaries from `docs/host.md`. Not derived from `$HOME`: under `sudo` it
/// is root's home, so the directory is passed explicitly.
fn binaries_dir() -> Option<PathBuf> {
    std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from)
}

fn missing_prerequisite(kernel: &Path) -> Option<String> {
    let is_root = std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0);
    if !is_root {
        return Some("the jailer must start as root".into());
    }
    let Some(bin) = binaries_dir() else {
        return Some("DYLOS_FC_BIN_DIR is not set (see docs/host.md)".into());
    };
    [
        Path::new("/dev/kvm"),
        &bin.join("jailer"),
        &bin.join("firecracker"),
        kernel,
    ]
    .into_iter()
    .find(|p| !p.exists())
    .map(|p| format!("{} is missing", p.display()))
}

/// A network namespace owned by the test: it lives as long as the holder process, which is
/// killed on drop. Stands in for the per-lab namespace of `dylos-net` until it lands.
struct OwnedNetns {
    holder: Child,
    path: PathBuf,
}

impl OwnedNetns {
    async fn new() -> Self {
        let mut holder = Command::new("unshare")
            .args(["--net", "sh", "-c", "echo ready; exec cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Printed after unshare(2), so the path below already names the new namespace.
        let mut ready = String::new();
        BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut ready)
            .await
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let path = PathBuf::from(format!("/proc/{}/ns/net", holder.id().unwrap()));
        Self { holder, path }
    }
}

fn netns_of(pid: u32) -> PathBuf {
    std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap()
}

fn spec(node: &str, kernel: &Path, netns: &OwnedNetns) -> VmSpec {
    VmSpec {
        lab_id: "e2e".into(),
        node: node.into(),
        netns: Some(netns.path.clone()),
        files: vec![ChrootFile::new(kernel, "vmlinux.bin")],
        timeouts: Timeouts::default(),
    }
}

/// Needs root and KVM; `just e2e` runs it.
#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn real_jailer_runs_firecracker_unprivileged_in_its_cgroup_and_netns() {
    let kernel = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernels/vmlinux.bin");
    if let Some(reason) = missing_prerequisite(&kernel) {
        eprintln!("SKIPPED real_jailer e2e test: {reason}");
        return;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bin = binaries_dir().unwrap();
    let mut config = JailerConfig::new(dir.path(), NOBODY, NOBODY);
    config.jailer = bin.join("jailer");
    config.firecracker = bin.join("firecracker");
    let netns = OwnedNetns::new().await;

    let mut vm = Vm::launch(&config, &spec("vm0", &kernel, &netns))
        .await
        .unwrap();
    let pid = vm.pid().unwrap();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let uid_line = status.lines().find(|l| l.starts_with("Uid:")).unwrap();
    assert!(
        uid_line.split_whitespace().skip(1).all(|u| u == "65534"),
        "{uid_line}"
    );
    let paths = vm.paths().clone();
    assert!(paths.cgroup_dir.is_dir(), "{:?}", paths.cgroup_dir);
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert_eq!(cgroup.trim(), format!("0::/firecracker/{}", paths.id));
    assert_eq!(netns_of(pid), netns_of(netns.holder.id().unwrap()));

    let client = vm.client();
    client
        .put_machine_config(&MachineConfiguration::new(1, 128))
        .await
        .unwrap();
    client
        .put_boot_source(&BootSource::new("/vmlinux.bin"))
        .await
        .unwrap();

    vm.shutdown().await.unwrap();
    assert!(!paths.jail_dir.exists());
    assert!(!paths.cgroup_dir.exists());

    // The Drop path: the supervisor kills, reaps and removes the jail and the real cgroup.
    let vm = Vm::launch(&config, &spec("vm1", &kernel, &netns))
        .await
        .unwrap();
    let proc_dir = PathBuf::from(format!("/proc/{}", vm.pid().unwrap()));
    let paths = vm.paths().clone();
    assert!(paths.cgroup_dir.is_dir());
    drop(vm);
    support::wait_until(
        "the dropped VM is reaped and its jail and cgroup removed",
        Duration::from_secs(5),
        || !proc_dir.exists() && !paths.jail_dir.exists() && !paths.cgroup_dir.exists(),
    )
    .await;
}

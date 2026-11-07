#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use support::{check_e2e_prerequisites, test_tempdir};

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use dylos_fc::config::{BootSource, InstanceActionInfo, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailerConfig};
use dylos_runtime::{Timeouts, Vm, VmSpec};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const NOBODY: u32 = 65534;

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
        vcpu_count: 1,
        mem_size_mib: 128,
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
    let Some(prereqs) = check_e2e_prerequisites(
        "real_jailer_runs_firecracker_unprivileged_in_its_cgroup_and_netns",
    ) else {
        return;
    };
    let kernel = prereqs.kernel;
    let bin = prereqs.bin;
    let dir = test_tempdir();
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

    assert_eq!(
        std::fs::read_to_string(paths.cgroup_dir.join("pids.max"))
            .unwrap()
            .trim(),
        "33"
    );
    assert_eq!(
        std::fs::read_to_string(paths.cgroup_dir.join("memory.max"))
            .unwrap()
            .trim(),
        "268435456"
    );

    let client = vm.client();
    client
        .put_machine_config(&MachineConfiguration::new(1, 128))
        .await
        .unwrap();
    client
        .put_boot_source(&BootSource::new("/vmlinux.bin"))
        .await
        .unwrap();

    client
        .put::<_, serde_json::Value>("/actions", &InstanceActionInfo::instance_start())
        .await
        .unwrap();
    let info: serde_json::Value = client.get("/").await.unwrap().unwrap();
    assert_eq!(info["state"], "Running");

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

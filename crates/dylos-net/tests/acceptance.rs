#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use dylos_core::LabSpec;
use dylos_net::{FabricPlan, LabNetwork, teardown};

const SANDBOX_ENV: &str = "DYLOS_NET_TEST_SANDBOX";

fn sandboxed(test: &str) -> bool {
    if std::env::var_os(SANDBOX_ENV).is_some() {
        return true;
    }
    let probe = Command::new("unshare").args(["-Urmn", "true"]).status();
    if !probe.is_ok_and(|s| s.success()) {
        return false;
    }
    let status = Command::new("unshare")
        .arg("-Urmn")
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env(SANDBOX_ENV, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed inside the sandbox");
    false
}

fn plan() -> FabricPlan {
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    FabricPlan::new("lab1", &spec).unwrap()
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn ip_link(netns: &Path, args: &[&str]) -> String {
    let out = Command::new("nsenter")
        .arg(format!("--net={}", netns.display()))
        .args(["ip", "-o", "-d", "link", "show"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn test_all_devices_are_up_and_configured() {
    if !sandboxed("test_all_devices_are_up_and_configured") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path()).await.unwrap();

        for bridge in ["br-left", "br-right"] {
            let info = ip_link(net.netns_path(), &[bridge]);
            assert!(
                info.contains(",UP>") || info.contains("<UP,"),
                "bridge {bridge} is not admin UP: {info}"
            );
            assert!(
                info.contains("mcast_snooping 0"),
                "bridge {bridge} has snooping enabled"
            );
        }

        for tap in ["tap-A-eth0", "tap-B-eth0", "tap-B-eth1", "tap-C-eth0"] {
            let info = ip_link(net.netns_path(), &[tap]);
            assert!(
                info.contains(",UP>") || info.contains("<UP,"),
                "tap {tap} is not admin UP: {info}"
            );
            assert!(info.contains("tun type tap"), "tap {tap} is not a tap");
        }

        net.teardown().await.unwrap();
    });
}

#[test]
fn test_teardown_after_midway_failure_and_double_teardown() {
    if !sandboxed("test_teardown_after_midway_failure_and_double_teardown") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let p = plan();
        let path = dir.path().join(p.netns_name());

        // Inject failure midway: we manually create the netns and ONE bridge,
        // simulating a crash before the rest could be created.
        std::fs::File::create(&path).unwrap();
        Command::new("unshare")
            .args([
                "-n",
                "mount",
                "--bind",
                "/proc/self/ns/net",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();

        // Create the bridge inside the netns
        let out = Command::new("nsenter")
            .arg(format!("--net={}", path.display()))
            .args(["ip", "link", "add", "br-left", "type", "bridge"])
            .output()
            .unwrap();
        assert!(out.status.success());

        // Now we call teardown. It should clean up the partial state.
        teardown(&p, dir.path()).await.unwrap();

        // Assert netns is gone.
        assert_netns_gone(&path);

        // Double teardown
        teardown(&p, dir.path()).await.unwrap();
    });
}

fn assert_netns_gone(path: &Path) {
    assert!(!path.exists(), "{} still exists", path.display());
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let path_str = path.to_str().unwrap();
    assert!(!mounts.contains(path_str), "{path_str} still mounted");
}

#[test]
fn test_caller_thread_is_not_moved_to_lab_netns() {
    if !sandboxed("test_caller_thread_is_not_moved_to_lab_netns") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let original_ns = std::fs::metadata("/proc/self/ns/net").unwrap().ino();

    block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path()).await.unwrap();
        let after_create_ns = std::fs::metadata("/proc/self/ns/net").unwrap().ino();
        assert_eq!(
            original_ns, after_create_ns,
            "Caller thread was moved to a different netns during create"
        );

        let lab_ns = std::fs::metadata(net.netns_path()).unwrap().ino();
        assert_ne!(original_ns, lab_ns, "Caller thread is in the lab netns");

        net.teardown().await.unwrap();
        let after_teardown_ns = std::fs::metadata("/proc/self/ns/net").unwrap().ino();
        assert_eq!(
            original_ns, after_teardown_ns,
            "Caller thread was moved to a different netns during teardown"
        );
    });

    let after_drop_ns = std::fs::metadata("/proc/self/ns/net").unwrap().ino();
    assert_eq!(
        original_ns, after_drop_ns,
        "Caller thread was moved to a different netns during drop"
    );
}

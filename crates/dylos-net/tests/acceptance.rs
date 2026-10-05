#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_net::{Error, FabricPlan, LabNetwork, teardown};

const SANDBOX_ENV: &str = "DYLOS_NET_TEST_SANDBOX";

fn sandboxed(test: &str) -> bool {
    if std::env::var_os(SANDBOX_ENV).is_some() {
        return true;
    }
    let probe = Command::new("unshare").args(["-Urmn", "true"]).status();
    if !probe.is_ok_and(|s| s.success()) {
        eprintln!("SKIPPED {test}: unprivileged user namespaces (`unshare -Urmn`) unavailable");
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
fn test_failed_create_rolls_back_then_teardown_is_idempotent() {
    if !sandboxed("test_failed_create_rolls_back_then_teardown_is_idempotent") {
        return;
    }
    // Only this sandbox's mount namespace sees it: opening the TUN device yields /dev/null, so
    // TUNSETIFF fails after the netns and both bridges exist.
    let hidden = Command::new("mount")
        .args(["--bind", "/dev/null", "/dev/net/tun"])
        .status()
        .unwrap();
    assert!(hidden.success(), "mount over /dev/net/tun failed: {hidden}");
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let p = plan();
        let path = dir.path().join(p.netns_name());

        let err = LabNetwork::create(p.clone(), dir.path()).await.unwrap_err();
        assert!(
            matches!(
                err,
                Error::Io {
                    op: "create tap",
                    ..
                }
            ),
            "{err}"
        );
        assert_netns_gone(&path);
        assert_eq!(caller_fabric_links(), "");

        teardown(&p, dir.path()).await.unwrap();
        teardown(&p, dir.path()).await.unwrap();
        assert_netns_gone(&path);
    });
}

fn netns_gone(path: &Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    !path.exists() && !mounts.contains(path.to_str().unwrap())
}

fn assert_netns_gone(path: &Path) {
    assert!(
        netns_gone(path),
        "{} still exists or is mounted",
        path.display()
    );
}

fn caller_fabric_links() -> String {
    ip_link(Path::new("/proc/thread-self/ns/net"), &[])
        .lines()
        .filter(|l| l.contains(": br-") || l.contains(": tap-"))
        .collect()
}

fn thread_netns() -> u64 {
    std::fs::metadata("/proc/thread-self/ns/net").unwrap().ino()
}

#[test]
fn test_caller_and_pool_threads_are_not_moved_to_lab_netns() {
    if !sandboxed("test_caller_and_pool_threads_are_not_moved_to_lab_netns") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let original_ns = thread_netns();
    // A single blocking thread: the one that waited on the namespace workers is the one reused.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let pool_ns = || async { tokio::task::spawn_blocking(thread_netns).await.unwrap() };

    runtime.block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path()).await.unwrap();
        assert_eq!(original_ns, thread_netns(), "caller moved by create");
        assert_eq!(original_ns, pool_ns().await, "pool thread moved by create");
        let lab_ns = std::fs::metadata(net.netns_path()).unwrap().ino();
        assert_ne!(original_ns, lab_ns, "the lab netns is the caller's");

        net.teardown().await.unwrap();
        assert_eq!(original_ns, thread_netns(), "caller moved by teardown");
        assert_eq!(
            original_ns,
            pool_ns().await,
            "pool thread moved by teardown"
        );

        let live = LabNetwork::create(plan(), dir.path()).await.unwrap();
        let path = live.netns_path().to_owned();
        drop(live);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !netns_gone(&path) {
            assert!(
                Instant::now() < deadline,
                "drop never removed {}",
                path.display()
            );
            std::thread::yield_now();
        }
        assert_eq!(original_ns, thread_netns(), "caller moved by drop");
        assert_eq!(original_ns, pool_ns().await, "pool thread moved by drop");
    });
}

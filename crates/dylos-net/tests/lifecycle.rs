//! Creates real netns, bridges and TAPs without privileges: each test re-executes itself under
//! `unshare --user --map-root-user --mount --net`, so it is root only in throwaway namespaces and
//! never touches the host network. Needs util-linux (`unshare`, `nsenter`) and iproute2.
#![allow(clippy::unwrap_used)] // helpers outside `#[test]` fns, see clippy.toml

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::task::Poll;
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_net::{Error, FabricPlan, LabNetwork, teardown};

const SANDBOX_ENV: &str = "DYLOS_NET_TEST_SANDBOX";
const NO_LINKS: [&str; 0] = [];
const REFERENCE_LINKS: [&str; 6] = [
    "br-left",
    "br-right",
    "tap-A-eth0",
    "tap-B-eth0",
    "tap-B-eth1",
    "tap-C-eth0",
];

/// True when running inside the sandbox, where the caller runs the test body. Outside, re-runs
/// this single test in the sandbox, asserts that it passed, and returns false.
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

/// `ip -o -d link show <args>` inside the netns at `netns`.
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

fn fabric_links(netns: &Path) -> Vec<String> {
    let mut links: Vec<String> = ip_link(netns, &[])
        .lines()
        .filter_map(|l| l.split(": ").nth(1))
        .map(|name| name.split('@').next().unwrap_or(name).to_owned())
        .filter(|name| name.starts_with("br-") || name.starts_with("tap-"))
        .collect();
    links.sort();
    links
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

/// For cleanups that run on a detached thread (`Drop`).
fn wait_until_netns_gone(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !netns_gone(path) {
        assert!(
            Instant::now() < deadline,
            "{} never removed",
            path.display()
        );
        std::thread::yield_now();
    }
}

#[test]
fn create_then_teardown_50_times_leaves_nothing() {
    if !sandboxed("create_then_teardown_50_times_leaves_nothing") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        for i in 0..50 {
            let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
            let path = net.netns_path().to_owned();
            assert_eq!(fabric_links(&path), REFERENCE_LINKS, "iteration {i}");
            net.teardown().await.unwrap();
            net.teardown().await.unwrap();
            assert_netns_gone(&path);
            assert_eq!(fabric_links(Path::new("/proc/self/ns/net")), NO_LINKS);
        }
    });
}

#[test]
fn bridges_have_multicast_snooping_disabled_and_taps_are_enslaved() {
    if !sandboxed("bridges_have_multicast_snooping_disabled_and_taps_are_enslaved") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        for bridge in ["br-left", "br-right"] {
            assert!(ip_link(net.netns_path(), &[bridge]).contains("mcast_snooping 0"));
        }
        let tap = ip_link(net.netns_path(), &["tap-B-eth1"]);
        assert!(tap.contains("master br-right"), "{tap}");
        assert!(tap.contains("tun type tap"), "{tap}");
        net.teardown().await.unwrap();
    });
}

#[test]
fn teardown_is_idempotent_without_or_after_partial_creation() {
    if !sandboxed("teardown_is_idempotent_without_or_after_partial_creation") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plan = plan();
    let path = dir.path().join(plan.netns_name());
    block_on(async {
        teardown(&plan, dir.path()).await.unwrap();

        // What a crash between creating the netns file and bind-mounting it leaves behind.
        std::fs::write(&path, b"").unwrap();
        teardown(&plan, dir.path()).await.unwrap();
        teardown(&plan, dir.path()).await.unwrap();
        assert_netns_gone(&path);
    });
}

#[test]
fn create_refuses_an_existing_netns_and_leaves_it_alone() {
    if !sandboxed("create_refuses_an_existing_netns_and_leaves_it_alone") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let mut first = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        let err = LabNetwork::create(plan(), dir.path(), 0, 0)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NetnsExists { .. }), "{err}");
        assert_eq!(fabric_links(first.netns_path()), REFERENCE_LINKS);
        first.teardown().await.unwrap();
    });
}

#[test]
fn teardown_deletes_devices_even_if_a_process_still_holds_the_netns() {
    if !sandboxed("teardown_deletes_devices_even_if_a_process_still_holds_the_netns") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        let mut leaked = Command::new("nsenter")
            .arg(format!("--net={}", net.netns_path().display()))
            .args(["sleep", "30"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let held = format!("/proc/{}/ns/net", leaked.id());
        let lab_ns = std::fs::metadata(net.netns_path()).unwrap().ino();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::metadata(&held).map(|m| m.ino()).ok() != Some(lab_ns) {
            assert!(
                Instant::now() < deadline,
                "child never entered the lab netns"
            );
            std::thread::yield_now();
        }

        net.teardown().await.unwrap();
        assert_netns_gone(net.netns_path());
        assert_eq!(fabric_links(Path::new(&held)), NO_LINKS);
        leaked.kill().unwrap();
        leaked.wait().unwrap();
    });
}

#[test]
fn dropping_a_live_lab_network_on_the_executor_tears_it_down() {
    if !sandboxed("dropping_a_live_lab_network_on_the_executor_tears_it_down") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = block_on(async {
        let net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        let path = net.netns_path().to_owned();
        drop(net);
        path
    });
    wait_until_netns_gone(&path);
    assert_eq!(fabric_links(Path::new("/proc/self/ns/net")), NO_LINKS);
}

#[test]
fn cancelled_create_leaves_nothing() {
    if !sandboxed("cancelled_create_leaves_nothing") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(plan().netns_name());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // The first poll hands the work to the blocking pool; the future is then dropped.
        let mut create = pin!(LabNetwork::create(plan(), dir.path(), 0, 0));
        std::future::poll_fn(|cx| {
            let _ = create.as_mut().poll(cx);
            Poll::Ready(())
        })
        .await;
    });
    // Dropping the runtime waits for the blocking pool, so the worker has finished: the netns
    // exists now only if its cleanup is still pending, and must disappear on its own.
    drop(runtime);
    wait_until_netns_gone(&path);
    assert_eq!(fabric_links(Path::new("/proc/self/ns/net")), NO_LINKS);
}

#[test]
fn a_stale_handle_never_tears_down_a_newer_network_with_the_same_name() {
    if !sandboxed("a_stale_handle_never_tears_down_a_newer_network_with_the_same_name") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        let mut first = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        first.teardown().await.unwrap();
        let mut second = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        first.teardown().await.unwrap();
        drop(first);
        assert_eq!(fabric_links(second.netns_path()), REFERENCE_LINKS);

        // Crash recovery by name while `second` is still live, then a new lab with the same id.
        teardown(&plan(), dir.path()).await.unwrap();
        let mut third = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        second.teardown().await.unwrap();
        drop(second);
        assert_eq!(fabric_links(third.netns_path()), REFERENCE_LINKS);

        third.teardown().await.unwrap();
        assert_netns_gone(third.netns_path());
    });
}

#[test]
fn taps_are_owned_by_the_requested_uid_and_gid() {
    if !sandboxed("taps_are_owned_by_the_requested_uid_and_gid") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    block_on(async {
        // The unshare harness maps only uid/gid 0; other owners are invalid in this user namespace.
        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        for tap in net.plan().taps() {
            let out = Command::new("nsenter")
                .arg(format!("--net={}", net.netns_path().display()))
                .args(["unshare", "--mount", "sh"])
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/tap-ownership.sh"
                ))
                .arg(&tap.name)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                "0\n0\n",
                "{}",
                tap.name
            );
        }
        let path = net.netns_path().to_owned();
        net.teardown().await.unwrap();
        net.teardown().await.unwrap();
        assert_netns_gone(&path);
        assert_eq!(fabric_links(Path::new("/proc/self/ns/net")), NO_LINKS);
    });
}

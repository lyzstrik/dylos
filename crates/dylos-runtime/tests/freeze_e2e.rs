#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};
use serde_json::Value;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

async fn guest(lab: &Lab, node: &str, command: &str) -> String {
    let out = lab.exec(node, command, COMMAND_TIMEOUT).await.unwrap();
    assert_eq!(out.exit_code, 0, "{node}: {command}: {}", out.stdout);
    out.stdout
}

async fn guest_counter(lab: &Lab, node: &str, path: &str) -> u64 {
    // Serial output can include asynchronous console messages alongside command output.
    let output = guest(
        lab,
        node,
        &format!("printf 'DYLOS_COUNTER=%s\\n' \"$(cat {path})\""),
    )
    .await;
    let values: Vec<u64> = output
        .lines()
        .filter_map(|line| line.strip_prefix("DYLOS_COUNTER="))
        .map(|value| value.parse().expect("invalid guest counter"))
        .collect();
    assert_eq!(values.len(), 1, "{node}: {path}: {output}");
    values[0]
}

async fn guest_rx(lab: &Lab) -> Vec<u64> {
    let mut counters = Vec::new();
    for (node, interface) in [("A", "eth0"), ("B", "eth0"), ("B", "eth1"), ("C", "eth0")] {
        counters.push(
            guest_counter(
                lab,
                node,
                &format!("/sys/class/net/{interface}/statistics/rx_packets"),
            )
            .await,
        );
    }
    counters
}

async fn tap_rx(namespace: &Path) -> Vec<u64> {
    let output = tokio::process::Command::new("nsenter")
        .arg(format!("--net={}", namespace.display()))
        .args(["ip", "-j", "-s", "link", "show"])
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(COMMAND_TIMEOUT, output)
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let links: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    ["tap-A-eth0", "tap-B-eth0", "tap-B-eth1", "tap-C-eth0"]
        .iter()
        .map(|name| {
            let link = links.iter().find(|link| link["ifname"] == *name).unwrap();
            link["stats64"]["rx"]["packets"].as_u64().unwrap()
        })
        .collect()
}

async fn wait_for_rx(lab: &Lab, before: u64) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if guest_rx(lab).await[3] > before + 100 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("existing TCP stream did not make progress");
}

async fn marker(lab: &Lab, family: &str, address: &str, tag: &str) {
    let version = family.trim_start_matches('-');
    let destination = if family == "-6" {
        format!("[{address}]")
    } else {
        address.to_owned()
    };
    guest(
        lab,
        "A",
        &format!("printf '%s\\n' {tag} | socat -u - UDP{version}-SENDTO:{destination}:55254"),
    )
    .await;
}

async fn wait_marker(lab: &Lab, family: &str, address: &str, tag: &str) {
    tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            marker(lab, family, address, tag).await;
            if lab
                .exec(
                    "C",
                    &format!("grep -q {tag} /tmp/freeze-markers"),
                    COMMAND_TIMEOUT,
                )
                .await
                .unwrap()
                .success()
            {
                break;
            }
        }
    })
    .await
    .expect("UDP marker receiver did not respond");
}

async fn hold_frozen(
    lab: &Lab,
    namespace: &Path,
    family: &str,
    address: &str,
    taps: &[u64],
    stable: &[u64],
) {
    // ICMP echo is originated only after T; background TCP also keeps retransmitting.
    let tx_before = guest_counter(lab, "A", "/sys/class/net/eth0/statistics/tx_packets").await;
    guest(lab, "A", &format!("sh -c 'rm -f /tmp/freeze-ping.status; (ping {family} -c 2 -W 1 {address} > /tmp/freeze-ping.log 2>&1; echo $? > /tmp/freeze-ping.status) &'")).await;
    marker(lab, family, address, "AFTER_T_HOLD").await;
    let held = Instant::now();
    while held.elapsed() < Duration::from_secs(2) {
        assert_eq!(guest_rx(lab).await, stable, "guest RX during frozen hold");
        assert_eq!(tap_rx(namespace).await, taps, "TAP RX during frozen hold");
    }
    let status = tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            let out = lab
                .exec("A", "cat /tmp/freeze-ping.status", COMMAND_TIMEOUT)
                .await
                .unwrap();
            if out.exit_code == 0 {
                break guest_counter(lab, "A", "/tmp/freeze-ping.status").await;
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(status, 0, "post-T echo received while frozen");
    let tx_after = guest_counter(lab, "A", "/sys/class/net/eth0/statistics/tx_packets").await;
    assert!(
        tx_after > tx_before,
        "no post-T guest transmission observed"
    );

    eprintln!(
        "{family}: post-T guest TX {tx_before} -> {tx_after}; frozen TAP RX {taps:?}; settled guest RX {stable:?}"
    );
}

async fn exercise(lab: &mut Lab, namespace: &Path, family: &str, address: &str) {
    let version = family.trim_start_matches('-');
    guest(lab, "C", &format!("sh -c 'socat -u UDP{version}-RECV:55254 - > /tmp/freeze-markers 2>/tmp/freeze-socat.log & echo $! > /tmp/freeze-socat.pid'")).await;
    wait_marker(lab, family, address, "BEFORE_T").await;
    guest(lab, "C", "iperf3 -s -D").await;
    guest(lab, "A", &format!("ping {family} -c 1 -W 2 {address}")).await;
    guest(lab, "A", &format!("sh -c 'iperf3 {family} -c {address} -P 4 -t 90 > /tmp/freeze-iperf.log 2>&1 & echo $! > /tmp/freeze-iperf.pid'")).await;
    wait_for_rx(lab, guest_rx(lab).await[3]).await;
    lab.network_mut().unwrap().freeze().await.unwrap();
    let frozen_at = Instant::now();
    marker(lab, family, address, "AFTER_T_EARLY").await;
    let initial = guest_rx(lab).await;
    let taps = tap_rx(namespace).await;

    // Pre-T TAP queues are allowed to drain. Observe this separately from the quiet window.
    let stable = tokio::time::timeout(Duration::from_secs(5), async {
        let mut previous = initial.clone();
        let mut since = Instant::now();
        loop {
            let current = guest_rx(lab).await;
            assert_eq!(tap_rx(namespace).await, taps, "post-T TAP ingress");
            if current != previous {
                since = Instant::now();
                previous = current;
            } else if since.elapsed() >= Duration::from_millis(200) {
                break previous;
            }
        }
    })
    .await
    .expect("pre-T RX did not settle");
    eprintln!(
        "{family}: freeze to settled RX {:?}; pre-T guest RX delta {:?} -> {:?}",
        frozen_at.elapsed(),
        initial,
        stable
    );

    hold_frozen(lab, namespace, family, address, &taps, &stable).await;

    // Exercise pause skew with A and B running; guest counters become readable on resume.
    lab.vm("C").unwrap().client().pause().await.unwrap();
    marker(lab, family, address, "AFTER_T_PAUSED").await;
    let probe = lab
        .exec(
            "A",
            &format!("ping {family} -c 2 -W 1 {address}"),
            COMMAND_TIMEOUT,
        )
        .await
        .unwrap();
    assert_ne!(probe.exit_code, 0);
    assert_eq!(tap_rx(namespace).await, taps);
    lab.vm("C").unwrap().client().resume().await.unwrap();
    assert_eq!(
        guest_rx(lab).await,
        stable,
        "RX changed across receiver pause"
    );
    guest(lab, "C", "! grep -q AFTER_T /tmp/freeze-markers").await;
    lab.network_mut().unwrap().thaw().await.unwrap();
    wait_for_rx(lab, stable[3]).await;
    wait_marker(lab, family, address, "THAW").await;
    guest(lab, "A", "kill -0 $(cat /tmp/freeze-iperf.pid)").await;
    guest(lab, "A", "kill $(cat /tmp/freeze-iperf.pid)").await;
    guest(lab, "C", "kill $(cat /tmp/freeze-socat.pid)").await;
    guest(lab, "C", "pkill iperf3").await;
    eprintln!(
        "{family}: unchanged TAP/guest RX during 2 s hold and receiver pause; original TCP flow resumed"
    );
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn abc_fabric_freeze_under_real_traffic() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED freeze_e2e: DYLOS_FC_BIN_DIR is not set");
        return;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("SKIPPED freeze_e2e: must start as root");
        return;
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
            eprintln!("SKIPPED freeze_e2e: {} is missing", path.display());
            return;
        }
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut jailer = JailerConfig::new(dir.path().join("jails"), 65534, 65534);
    jailer.jailer = bin.join("jailer");
    jailer.firecracker = bin.join("firecracker");
    let config = LabConfig {
        store: dylos_store::Store {
            base: dir.path().join("labs"),
            image,
        },
        jailer,
        kernel,
        netns_dir: dir.path().join("netns"),
        ..LabConfig::default()
    };
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    let mut lab = Lab::up_with(&config, &spec, "freeze-e2e").await.unwrap();
    let namespace = lab.network_mut().unwrap().netns_path().to_owned();
    let paths: Vec<_> = lab
        .vms()
        .iter()
        .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
        .collect();
    let root = lab.root().to_owned();
    // Teardown is awaited even when an assertion in the measurement fails.
    let run = tokio::spawn(async move {
        use futures_util::FutureExt;
        let result = std::panic::AssertUnwindSafe(async {
            exercise(&mut lab, &namespace, "-6", "fd64:796c:6f73:2::2").await;
            exercise(&mut lab, &namespace, "-4", "10.0.2.2").await;
        })
        .catch_unwind()
        .await;
        lab.teardown().await.unwrap();
        lab.teardown().await.unwrap();
        assert!(!namespace.exists());
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    });
    run.await.unwrap();
    assert!(!root.exists());
    for (paths, pid) in paths {
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!paths.jail_dir.exists());
        assert!(!paths.cgroup_dir.exists());
    }
    eprintln!("{}", include_str!("fixtures/freeze-proof-obligations.txt"));
}

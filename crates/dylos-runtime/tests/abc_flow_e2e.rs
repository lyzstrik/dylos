#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::panic
)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::FutureExt;

use dylos_core::LabSpec;
use dylos_fc::config::{BootSource, Drive, InstanceActionInfo, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailPaths, JailerConfig};
use dylos_runtime::{Error, Lab, LabConfig, LabError, Timeouts, Vm, VmSpec};

fn binaries_dir() -> Option<PathBuf> {
    std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from)
}

fn missing_prerequisite(kernel: &Path, rootfs: &Path) -> Option<String> {
    let is_root = std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0);
    if !is_root {
        return Some("must start as root".into());
    }
    let Some(bin) = binaries_dir() else {
        return Some("DYLOS_FC_BIN_DIR is not set".into());
    };
    for path in [
        Path::new("/dev/kvm"),
        &bin.join("jailer"),
        &bin.join("firecracker"),
        kernel,
        rootfs,
    ] {
        if !path.exists() {
            return Some(format!("{} is missing", path.display()));
        }
    }
    None
}

/// Parses global IP addresses assigned to network interfaces from `ip -o addr show`.
fn parse_global_ips(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Format: <num>: <dev> inet|inet6 <addr/prefix> scope global ...
            if parts.len() >= 6 && parts.contains(&"scope") {
                let scope_idx = parts.iter().position(|&p| p == "scope")?;
                if parts.get(scope_idx + 1) == Some(&"global") {
                    return parts.get(3).map(|s| (*s).to_string());
                }
            }
            None
        })
        .collect()
}

async fn assert_interface_addresses(
    lab: &Lab,
    node: &str,
    dev: &str,
    expected_v6: &[&str],
    expected_v4: &[&str],
) {
    let out_v6 = lab
        .exec(
            node,
            &format!("ip -o -6 addr show dev {dev} scope global"),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        out_v6.success(),
        "IPv6 address query for {node}:{dev} failed: {out_v6:?}"
    );
    let mut actual_v6 = parse_global_ips(&out_v6.stdout);
    actual_v6.sort();
    let mut exp_v6: Vec<String> = expected_v6.iter().map(|&s| s.to_string()).collect();
    exp_v6.sort();
    assert_eq!(
        actual_v6, exp_v6,
        "Mismatch for IPv6 addresses on {node}:{dev}"
    );

    let out_v4 = lab
        .exec(
            node,
            &format!("ip -o -4 addr show dev {dev} scope global"),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        out_v4.success(),
        "IPv4 address query for {node}:{dev} failed: {out_v4:?}"
    );
    let mut actual_v4 = parse_global_ips(&out_v4.stdout);
    actual_v4.sort();
    let mut exp_v4: Vec<String> = expected_v4.iter().map(|&s| s.to_string()).collect();
    exp_v4.sort();
    assert_eq!(
        actual_v4, exp_v4,
        "Mismatch for IPv4 addresses on {node}:{dev}"
    );
}

/// Moved from LYZ-16: Checks that each VM has exactly the declared addresses per interface.
async fn verify_guest_declared_addresses(lab: &Lab) {
    assert_interface_addresses(
        lab,
        "A",
        "eth0",
        &["fd64:796c:6f73:1::2/64"],
        &["10.0.1.2/24"],
    )
    .await;

    assert_interface_addresses(
        lab,
        "B",
        "eth0",
        &["fd64:796c:6f73:1::1/64"],
        &["10.0.1.1/24"],
    )
    .await;
    assert_interface_addresses(
        lab,
        "B",
        "eth1",
        &["fd64:796c:6f73:2::1/64"],
        &["10.0.2.1/24"],
    )
    .await;

    assert_interface_addresses(
        lab,
        "C",
        "eth0",
        &["fd64:796c:6f73:2::2/64"],
        &["10.0.2.2/24"],
    )
    .await;
}

/// Asserts that a ping command's summary line reports exactly 0% packet loss.
fn assert_no_packet_loss(stdout: &str, context: &str) {
    let mut found = false;
    for line in stdout.lines() {
        if let Some(idx) = line.find("% packet loss").or_else(|| line.find("% loss")) {
            let prefix = line[..idx].trim_end();
            let loss_str = prefix
                .rsplit(|c: char| c.is_whitespace() || c == ',')
                .next()
                .unwrap_or("");
            assert!(
                loss_str == "0" || loss_str == "0.0",
                "{context}: expected 0% packet loss, found {loss_str}% in summary line: {line}\nFull output:\n{stdout}"
            );
            found = true;
            break;
        }
    }
    assert!(
        found,
        "{context}: packet loss summary line not found in output:\n{stdout}"
    );
}

/// Readiness: Usable connectivity across the lab in both families (ADR-0002: -6 first, then -4).
async fn verify_ping_connectivity(lab: &Lab) {
    let ping_v6 = lab
        .exec(
            "A",
            "ping -6 -c 3 -W 2 fd64:796c:6f73:2::2",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        ping_v6.success(),
        "IPv6 ping from A to C failed: {}",
        ping_v6.stdout
    );
    assert_no_packet_loss(&ping_v6.stdout, "IPv6 ping from A to C");

    let ping_v4 = lab
        .exec("A", "ping -4 -c 3 -W 2 10.0.2.2", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        ping_v4.success(),
        "IPv4 ping from A to C failed: {}",
        ping_v4.stdout
    );
    assert_no_packet_loss(&ping_v4.stdout, "IPv4 ping from A to C");
}

/// iperf3 flow from A to C for 10 s, throughput logged (ADR-0002: -6 first, then -4).
async fn verify_iperf3_throughput(lab: &Lab) {
    // Start iperf3 server daemon on C for IPv6.
    let srv_v6 = lab
        .exec(
            "C",
            "killall iperf3 2>/dev/null || true; iperf3 -6 -s -D",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        srv_v6.success(),
        "Failed to start iperf3 server on C for IPv6"
    );

    let flow_v6 = lab
        .exec(
            "A",
            "iperf3 -6 -c fd64:796c:6f73:2::2 -t 10",
            Duration::from_secs(25),
        )
        .await
        .unwrap();
    assert!(
        flow_v6.success(),
        "IPv6 iperf3 flow failed: {}",
        flow_v6.stdout
    );
    eprintln!("=== iperf3 IPv6 flow (10s) ===\n{}", flow_v6.stdout);
    tracing::info!(throughput = %flow_v6.stdout, "iperf3 IPv6 throughput");

    // Restart iperf3 server daemon on C for IPv4.
    let srv_v4 = lab
        .exec(
            "C",
            "killall iperf3 2>/dev/null || true; iperf3 -4 -s -D",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        srv_v4.success(),
        "Failed to start iperf3 server on C for IPv4"
    );

    let flow_v4 = lab
        .exec("A", "iperf3 -4 -c 10.0.2.2 -t 10", Duration::from_secs(25))
        .await
        .unwrap();
    assert!(
        flow_v4.success(),
        "IPv4 iperf3 flow failed: {}",
        flow_v4.stdout
    );
    eprintln!("=== iperf3 IPv4 flow (10s) ===\n{}", flow_v4.stdout);
    tracing::info!(throughput = %flow_v4.stdout, "iperf3 IPv4 throughput");

    let _ = lab
        .exec(
            "C",
            "killall iperf3 2>/dev/null || true",
            Duration::from_secs(5),
        )
        .await;
}

/// A cannot reach C if B disables routing (proves traffic really goes through B).
async fn verify_routing_through_b(lab: &Lab) {
    let dis = lab
        .exec(
            "B",
            "sysctl -qw net.ipv6.conf.all.forwarding=0 net.ipv4.ip_forward=0",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(dis.success(), "Failed to disable routing on B");

    let drop_v6 = lab
        .exec(
            "A",
            "ping -6 -c 1 -W 1 fd64:796c:6f73:2::2",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        !drop_v6.success(),
        "IPv6 ping succeeded when routing was disabled on B!"
    );

    let drop_v4 = lab
        .exec("A", "ping -4 -c 1 -W 1 10.0.2.2", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        !drop_v4.success(),
        "IPv4 ping succeeded when routing was disabled on B!"
    );

    // Re-enable routing on B and confirm traffic flows again.
    let en = lab
        .exec(
            "B",
            "sysctl -qw net.ipv6.conf.all.forwarding=1 net.ipv4.ip_forward=1",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(en.success(), "Failed to re-enable routing on B");

    let rec_v6 = lab
        .exec(
            "A",
            "ping -6 -c 1 -W 2 fd64:796c:6f73:2::2",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        rec_v6.success(),
        "IPv6 ping failed after re-enabling routing on B: {}",
        rec_v6.stdout
    );
    assert_no_packet_loss(&rec_v6.stdout, "IPv6 recovery ping");

    let rec_v4 = lab
        .exec("A", "ping -4 -c 1 -W 2 10.0.2.2", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        rec_v4.success(),
        "IPv4 ping failed after re-enabling routing on B: {}",
        rec_v4.stdout
    );
    assert_no_packet_loss(&rec_v4.stdout, "IPv4 recovery ping");
}

/// Teardown and assert nothing left on the host.
async fn verify_teardown_leaves_nothing(
    mut lab: Lab,
    paths: Vec<(JailPaths, u32)>,
    root: PathBuf,
    netns: PathBuf,
) {
    lab.teardown().await.unwrap();

    for (p, pid) in paths {
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!p.jail_dir.exists());
        assert!(!p.cgroup_dir.exists());
    }
    assert!(!root.exists());
    assert!(!netns.exists());
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn abc_lab_ping_and_iperf3_flow_through_b_then_teardown_leaves_nothing() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");

    if let Some(reason) = missing_prerequisite(&kernel, &image) {
        eprintln!("SKIPPED abc_flow_e2e: {reason}");
        return;
    }

    let bin = binaries_dir().unwrap();
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
        boot_timeout: Duration::from_secs(30),
        ..LabConfig::default()
    };

    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    let lab_id = "abc-flow-e2e";
    let lab = Lab::up_with(&config, &spec, lab_id)
        .await
        .expect("lab bring-up failed");
    assert_eq!(lab.vms().len(), 3);

    let paths: Vec<_> = lab
        .vms()
        .iter()
        .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
        .collect();
    let root = lab.root().to_owned();
    let netns = config.netns_dir.join(format!("dylos-{lab_id}"));

    let test_result = std::panic::AssertUnwindSafe(async {
        verify_guest_declared_addresses(&lab).await;
        verify_ping_connectivity(&lab).await;
        verify_iperf3_throughput(&lab).await;
        verify_routing_through_b(&lab).await;
    })
    .catch_unwind()
    .await;

    verify_teardown_leaves_nothing(lab, paths, root, netns).await;

    if let Err(payload) = test_result {
        std::panic::resume_unwind(payload);
    }
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn guest_with_failed_network_marker_reports_not_ready() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");

    if let Some(reason) = missing_prerequisite(&kernel, &image) {
        eprintln!("SKIPPED guest_with_failed_network_marker_reports_not_ready: {reason}");
        return;
    }

    let bin = binaries_dir().unwrap();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut config = JailerConfig::new(dir.path(), 65534, 65534);
    config.jailer = bin.join("jailer");
    config.firecracker = bin.join("firecracker");

    let spec = VmSpec {
        vcpu_count: 1,
        mem_size_mib: 128,
        lab_id: "fail-e2e".into(),
        node: "vm0".into(),
        netns: None,
        files: vec![
            ChrootFile::new(&kernel, "vmlinux.bin"),
            ChrootFile::new(&image, "rootfs.ext4"),
        ],
        timeouts: Timeouts::default(),
    };

    let mut vm = Vm::launch(&config, &spec).await.unwrap();
    let paths = vm.paths().clone();
    let pid = vm.pid().unwrap();

    let test_result = std::panic::AssertUnwindSafe(async {
        let client = vm.client();
        client
            .put_machine_config(&MachineConfiguration::new(1, 128))
            .await
            .unwrap();

        // Configure boot source with an unmatching MAC address in dylos.if to force net-setup failure.
        let mut boot_source = BootSource::new("/vmlinux.bin");
        boot_source.boot_args = Some(
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda dylos.if=eth0,02:ff:ff:ff:ff:ff,fd64::1/64"
                .into(),
        );
        client.put_boot_source(&boot_source).await.unwrap();

        let mut drive = Drive::new("rootfs", true);
        drive.path_on_host = Some("/rootfs.ext4".into());
        drive.is_read_only = Some(false);
        client.put_drive(&drive).await.unwrap();

        client
            .put::<_, serde_json::Value>("/actions", &InstanceActionInfo::instance_start())
            .await
            .unwrap();

        // net-setup-guard catches net-setup failure, touches /run/dylos-net-failed,
        // report-readiness prints 'dylos: network setup failed', and wait_for_ready fails.
        let res = vm.wait_for_ready(Duration::from_secs(15)).await;
        assert!(
            matches!(res, Err(Error::ReadinessFailed { ref line, .. }) if line == "dylos: network setup failed"),
            "Expected ReadinessFailed but got: {res:?}"
        );
    })
    .catch_unwind()
    .await;

    let shutdown_res = vm.shutdown().await;
    assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    assert!(!paths.jail_dir.exists());
    assert!(!paths.cgroup_dir.exists());
    assert!(!paths.api_socket().exists());
    shutdown_res.unwrap();

    if let Err(payload) = test_result {
        std::panic::resume_unwind(payload);
    }
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn guest_with_unusable_connectivity_reports_not_ready() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");

    if let Some(reason) = missing_prerequisite(&kernel, &image) {
        eprintln!("SKIPPED guest_with_unusable_connectivity_reports_not_ready: {reason}");
        return;
    }

    let bin = binaries_dir().unwrap();
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
        boot_timeout: Duration::from_secs(30),
        ..LabConfig::default()
    };

    let spec = LabSpec::from_yaml_str(include_str!("fixtures/unusable_connectivity.yaml")).unwrap();
    let lab_id = "fail-connectivity-e2e";
    let root = config.store.base.join(lab_id);
    let netns = config.netns_dir.join(format!("dylos-{lab_id}"));
    let jail_paths = JailPaths::new(&config.jailer, lab_id, "A").unwrap();

    let test_result = std::panic::AssertUnwindSafe(async {
        // Guest network setup succeeds in assigning IP addresses and routes,
        // but gateway ping fails during readiness check. Lab::up_with must fail
        // with ReadinessFailed and automatically roll back host resources.
        let res = Lab::up_with(&config, &spec, lab_id).await;
        match res {
            Err(LabError::Vm(Error::ReadinessFailed { line, .. })) => {
                assert_eq!(line, "dylos: network setup failed");
            }
            Ok(mut lab) => {
                let _ = lab.teardown().await;
                panic!("Lab::up_with unexpectedly succeeded despite unreachable gateway");
            }
            Err(err) => {
                panic!("Expected ReadinessFailed from unreachable gateway, got: {err:?}");
            }
        }
    })
    .catch_unwind()
    .await;

    // Verify automatic rollback left nothing on the host.
    assert!(
        !netns.exists(),
        "Network namespace was not cleaned up: {netns:?}"
    );
    assert!(!root.exists(), "Storage root was not cleaned up: {root:?}");
    assert!(
        !jail_paths.jail_dir.exists(),
        "Jail directory was not cleaned up: {:?}",
        jail_paths.jail_dir
    );
    assert!(
        !jail_paths.cgroup_dir.exists(),
        "Cgroup directory was not cleaned up: {:?}",
        jail_paths.cgroup_dir
    );

    if let Err(payload) = test_result {
        std::panic::resume_unwind(payload);
    }
}

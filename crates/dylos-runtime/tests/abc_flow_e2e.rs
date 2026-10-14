#![allow(clippy::unwrap_used, clippy::expect_used, clippy::similar_names)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_fc::config::{BootSource, Drive, InstanceActionInfo, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailPaths, JailerConfig};
use dylos_runtime::{Error, Lab, LabConfig, Timeouts, Vm, VmSpec};

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

/// Moved from LYZ-16: Checks that each VM has exactly the declared addresses inside the guests.
async fn verify_guest_declared_addresses(lab: &Lab) {
    let addrs_a_v6 = lab
        .exec(
            "A",
            "ip -o -6 addr show scope global",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let addrs_a_v4 = lab
        .exec(
            "A",
            "ip -o -4 addr show scope global",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_global_ips(&addrs_a_v6.stdout),
        vec!["fd64:796c:6f73:1::2/64"]
    );
    assert_eq!(parse_global_ips(&addrs_a_v4.stdout), vec!["10.0.1.2/24"]);

    let addrs_b_v6 = lab
        .exec(
            "B",
            "ip -o -6 addr show scope global",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let addrs_b_v4 = lab
        .exec(
            "B",
            "ip -o -4 addr show scope global",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let mut b_v6_ips = parse_global_ips(&addrs_b_v6.stdout);
    b_v6_ips.sort();
    assert_eq!(
        b_v6_ips,
        vec!["fd64:796c:6f73:1::1/64", "fd64:796c:6f73:2::1/64"]
    );
    let mut b_v4_ips = parse_global_ips(&addrs_b_v4.stdout);
    b_v4_ips.sort();
    assert_eq!(b_v4_ips, vec!["10.0.1.1/24", "10.0.2.1/24"]);

    let addrs_c_v6 = lab
        .exec(
            "C",
            "ip -o -6 addr show scope global",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let addrs_c_v4 = lab
        .exec(
            "C",
            "ip -o -4 addr show scope global",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_global_ips(&addrs_c_v6.stdout),
        vec!["fd64:796c:6f73:2::2/64"]
    );
    assert_eq!(parse_global_ips(&addrs_c_v4.stdout), vec!["10.0.2.2/24"]);
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
    assert!(
        ping_v6.stdout.contains("0% packet loss") || ping_v6.stdout.contains("0% loss"),
        "IPv6 ping had packet loss: {}",
        ping_v6.stdout
    );

    let ping_v4 = lab
        .exec("A", "ping -4 -c 3 -W 2 10.0.2.2", Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        ping_v4.success(),
        "IPv4 ping from A to C failed: {}",
        ping_v4.stdout
    );
    assert!(
        ping_v4.stdout.contains("0% packet loss") || ping_v4.stdout.contains("0% loss"),
        "IPv4 ping had packet loss: {}",
        ping_v4.stdout
    );
}

/// iperf3 flow from A to C for 10 s, throughput logged (ADR-0002: -6 first, then -4).
async fn verify_iperf3_throughput(lab: &Lab) {
    // Start iperf3 server daemon on C for IPv6.
    let srv_v6 = lab
        .exec(
            "C",
            "killall iperf3 2>/dev/null || true; iperf3 -s -D",
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
            "killall iperf3 2>/dev/null || true; iperf3 -s -D",
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

    let rec_v4 = lab
        .exec("A", "ping -4 -c 1 -W 2 10.0.2.2", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        rec_v4.success(),
        "IPv4 ping failed after re-enabling routing on B: {}",
        rec_v4.stdout
    );
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

    verify_guest_declared_addresses(&lab).await;
    verify_ping_connectivity(&lab).await;
    verify_iperf3_throughput(&lab).await;
    verify_routing_through_b(&lab).await;

    let paths: Vec<_> = lab
        .vms()
        .iter()
        .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
        .collect();
    let root = lab.root().to_owned();
    let netns = config.netns_dir.join(format!("dylos-{lab_id}"));

    verify_teardown_leaves_nothing(lab, paths, root, netns).await;
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

    let client = vm.client();
    client
        .put_machine_config(&MachineConfiguration::new(1, 128))
        .await
        .unwrap();

    // Configure boot source with an invalid MAC address in dylos.if to force net-setup failure.
    let mut boot_source = BootSource::new("/vmlinux.bin");
    boot_source.boot_args = Some(
        "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda dylos.if=eth0,00:00:00:00:00:00,fd64::1/64"
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

    vm.shutdown().await.unwrap();

    assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    assert!(!paths.jail_dir.exists());
    assert!(!paths.cgroup_dir.exists());
    assert!(!paths.api_socket().exists());
}

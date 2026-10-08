#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn abc_lab_ready_then_control_socket_teardown_leaves_nothing() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED lab_e2e: DYLOS_FC_BIN_DIR is not set");
        return;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("SKIPPED lab_e2e: must start as root");
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
            eprintln!("SKIPPED lab_e2e: {} is missing", path.display());
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
        boot_timeout: Duration::from_secs(30),
        ..LabConfig::default()
    };
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    eprintln!(
        "lab_e2e: config={config:?}; netns={:?}; mountns={:?}",
        std::fs::read_link("/proc/self/ns/net"),
        std::fs::read_link("/proc/self/ns/mnt")
    );
    let result = Lab::up_with(&config, &spec, "abc-e2e").await;
    if let Err(error) = &result {
        eprintln!("lab bring-up failed: {error}\n{error:#?}");
    }
    let lab = result.expect("lab bring-up failed; see diagnostics above");
    assert_eq!(lab.vms().len(), 3);
    let paths: Vec<_> = lab
        .vms()
        .iter()
        .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
        .collect();
    let socket = lab.control_socket();
    let root = lab.root().to_owned();
    assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
    let server = tokio::spawn(lab.supervise());
    Lab::request_teardown(&socket, Duration::from_secs(60))
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    for (paths, pid) in paths {
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!paths.jail_dir.exists());
        assert!(!paths.cgroup_dir.exists());
    }
    assert!(!root.exists());
    assert!(!socket.exists());
    let netns = config.netns_dir.join("dylos-abc-e2e");
    assert!(!netns.exists());
    assert!(
        !std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .contains(netns.to_str().unwrap())
    );
    Lab::teardown_by_name(&config, "abc-e2e").await.unwrap();
}

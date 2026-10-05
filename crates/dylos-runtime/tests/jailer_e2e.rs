use std::os::unix::fs::MetadataExt;
use std::path::Path;

use dylos_fc::config::{BootSource, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailerConfig};
use dylos_runtime::{Timeouts, Vm, VmSpec};

const NOBODY: u32 = 65534;

fn missing_prerequisite(kernel: &Path) -> Option<String> {
    let is_root = std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0);
    if !is_root {
        return Some("the jailer must start as root".into());
    }
    [Path::new("/dev/kvm"), Path::new("/usr/bin/jailer"), kernel]
        .into_iter()
        .find(|p| !p.exists())
        .map(|p| format!("{} is missing", p.display()))
}

/// Needs root and KVM: run it with `just e2e`.
#[tokio::test]
async fn real_jailer_runs_firecracker_unprivileged_with_jail_relative_paths() {
    let kernel = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernels/vmlinux.bin");
    if let Some(reason) = missing_prerequisite(&kernel) {
        eprintln!("SKIPPED real_jailer e2e test: {reason}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = JailerConfig::new(dir.path(), NOBODY, NOBODY);
    let spec = VmSpec {
        lab_id: "e2e".into(),
        node: "vm0".into(),
        netns: None,
        files: vec![ChrootFile::new(&kernel, "vmlinux.bin")],
        timeouts: Timeouts::default(),
    };
    let mut vm = Vm::launch(&config, &spec).await.unwrap();
    let status = std::fs::read_to_string(format!("/proc/{}/status", vm.pid().unwrap())).unwrap();
    let uid_line = status.lines().find(|l| l.starts_with("Uid:")).unwrap();
    assert!(
        uid_line.split_whitespace().skip(1).all(|u| u == "65534"),
        "{uid_line}"
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

    let paths = vm.paths().clone();
    vm.shutdown().await.unwrap();
    assert!(!paths.jail_dir.exists());
    assert!(!paths.cgroup_dir.exists());
}

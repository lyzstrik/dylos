#![allow(clippy::unwrap_used, clippy::expect_used)]

use dylos_fc::config::{BootSource, Drive, InstanceActionInfo, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailerConfig};
use dylos_runtime::{Timeouts, Vm, VmSpec};
use std::path::{Path, PathBuf};
use std::time::Duration;

const NOBODY: u32 = 65534;

fn binaries_dir() -> Option<PathBuf> {
    std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from)
}

fn missing_prerequisite(kernel: &Path, rootfs: &Path) -> Option<String> {
    let is_root = std::fs::metadata("/proc/self").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    });
    if !is_root {
        return Some("must start as root".into());
    }
    let Some(bin) = binaries_dir() else {
        return Some("DYLOS_FC_BIN_DIR is not set".into());
    };
    [
        Path::new("/dev/kvm"),
        &bin.join("jailer"),
        &bin.join("firecracker"),
        kernel,
        rootfs,
    ]
    .into_iter()
    .find(|p| !p.exists())
    .map(|p| format!("{} is missing", p.display()))
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn vm_boots_and_reaches_readiness() {
    let kernel = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernels/vmlinux.bin");
    let rootfs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/images/rootfs.ext4");
    if let Some(reason) = missing_prerequisite(&kernel, &rootfs) {
        eprintln!("SKIPPED boot_e2e test: {reason}");
        return;
    }

    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bin = binaries_dir().unwrap();
    let mut config = JailerConfig::new(dir.path(), NOBODY, NOBODY);
    config.jailer = bin.join("jailer");
    config.firecracker = bin.join("firecracker");

    let spec = VmSpec {
        vcpu_count: 1,
        mem_size_mib: 128,
        lab_id: "boot-e2e".into(),
        node: "vm0".into(),
        netns: None,
        files: vec![
            ChrootFile::new(&kernel, "vmlinux.bin"),
            ChrootFile::new(&rootfs, "rootfs.ext4"),
        ],
        timeouts: Timeouts::default(),
    };

    let mut vm = Vm::launch(&config, &spec).await.unwrap();
    let paths = vm.paths().clone();

    let pid = vm.pid().unwrap();
    let res = async {
        let client = vm.client();
        client
            .put_machine_config(&MachineConfiguration::new(1, 128))
            .await
            .map_err(|e| e.to_string())?;

        let mut boot_source = BootSource::new("/vmlinux.bin");
        boot_source.boot_args = Some("console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda".into());
        client
            .put_boot_source(&boot_source)
            .await
            .map_err(|e| e.to_string())?;

        let mut drive = Drive::new("rootfs", true);
        drive.path_on_host = Some("/rootfs.ext4".into());
        client.put_drive(&drive).await.map_err(|e| e.to_string())?;

        client
            .put::<_, serde_json::Value>("/actions", &InstanceActionInfo::instance_start())
            .await
            .map_err(|e| e.to_string())?;

        // Wait for the guest to respond. We monitor the serial output for the agreed signal.
        // This shows that the guest kernel has booted, user space init has run, and it's ready.
        let boot_time = vm
            .wait_for_line(
                "dylos: ready",
                "dylos: network setup failed",
                Duration::from_secs(10),
            )
            .await
            .map_err(|e| e.to_string())?;

        Ok::<_, String>(boot_time)
    }
    .await;

    vm.shutdown().await.unwrap();

    assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    assert!(!paths.jail_dir.exists());
    assert!(!paths.cgroup_dir.exists());
    assert!(!paths.api_socket().exists());

    let boot_time = res.expect("Guest did not reach readiness in time or API failed");
    println!("Boot time: {} ms", boot_time.as_millis());
}

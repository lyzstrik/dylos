#![allow(clippy::unwrap_used, clippy::expect_used)]

use dylos_fc::config::{BootSource, Drive, InstanceActionInfo, MachineConfiguration};
use dylos_runtime::jailer::{ChrootFile, JailerConfig};
use dylos_runtime::{Timeouts, Vm, VmSpec};
use std::path::PathBuf;
use std::time::Duration;

mod support;
use support::{Captured, check_e2e_prerequisites, test_tempdir};

const NOBODY: u32 = 65534;

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn vm_boots_and_reaches_readiness() {
    let Some(prereqs) = check_e2e_prerequisites("vm_boots_and_reaches_readiness") else {
        return;
    };
    let kernel = prereqs.kernel;
    let rootfs = prereqs.image;
    let bin = prereqs.bin;

    let dir = test_tempdir();
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
    let (logs, _guard) = Captured::install();
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
            .wait_for_ready(Duration::from_secs(10))
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
    assert!(logs.contains("VM boot ready"));
    assert!(logs.contains("duration_ms="));
    println!("Boot time: {} ms", boot_time.as_millis());
}

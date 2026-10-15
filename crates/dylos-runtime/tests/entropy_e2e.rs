#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};
use futures_util::FutureExt;

async fn exec(lab: &Lab, node: &str, command: &str) -> String {
    let result = lab
        .exec(node, command, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(result.success(), "{node}: {command}: {result:?}");
    result.stdout.trim().to_owned()
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn clones_reseed_kernel_rng_via_vmgenid() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED entropy_e2e: DYLOS_FC_BIN_DIR missing");
        return;
    };
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");
    if std::fs::metadata("/proc/self").unwrap().uid() != 0
        || [
            Path::new("/dev/kvm"),
            &kernel,
            &image,
            &bin.join("jailer"),
            &bin.join("firecracker"),
        ]
        .iter()
        .any(|p| !p.exists())
    {
        eprintln!("SKIPPED entropy_e2e: requires root, KVM and built images/binaries");
        return;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut jailer = JailerConfig::new(dir.path().join("j"), 65534, 65534);
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

    let mut spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    for node in &mut spec.nodes {
        node.memory = 128;
        node.vcpus = 1;
    }
    let mut original = Lab::up_with(&config, &spec, "esrc").await.unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        let baseline = exec(&original, "A", "cat /proc/sys/kernel/random/uuid").await;
        let notices = exec(&original, "A",
            "dmesg | awk '/crng reseeded due to virtual machine fork/ {n++} END {print n+0}'").await;
        original.snapshot("snap").await.unwrap();
        let snapshot = config.store.snapshot_dir("esrc", "snap");
        let mut clones = Lab::fork_with(&config, &snapshot,
            &["ec1".into(), "ec2".into()]).await.unwrap();
        let assertions = std::panic::AssertUnwindSafe(async {
            let mut values = Vec::new();
            for clone in &clones {
                // Wait for a new kernel event, not a notice inherited in guest memory.
                let command = format!(
                    "while [ \"$(dmesg | grep -c 'crng reseeded due to virtual machine fork')\" -le {notices} ]; do :; done; cat /proc/sys/kernel/random/uuid"
                );
                let uuid = exec(clone, "A", &command).await;
                assert_eq!(uuid.len(), 36);
                assert_ne!(uuid, baseline);
                values.push(uuid);
                let bytes = exec(clone, "A", "od -An -N32 -tx1 /dev/urandom").await;
                assert_ne!(bytes, "");
                values.push(bytes);
                eprintln!("VMGENID_RESEED verified for {}", clone.id());
            }
            assert_ne!(values[0], values[2]);
            assert_ne!(values[1], values[3]);
        }).catch_unwind().await;
        for clone in &mut clones {
            let id = clone.id().to_owned();
            clone.teardown().await.unwrap();
            clone.teardown().await.unwrap();
            assert!(!config.store.base.join(&id).exists());
            assert!(!config.netns_dir.join(format!("dylos-{id}")).exists());
        }
        if let Err(payload) = assertions { std::panic::resume_unwind(payload); }
    }).catch_unwind().await;
    original.teardown().await.unwrap();
    assert!(!config.store.base.join("esrc").exists());
    assert!(!config.netns_dir.join("dylos-esrc").exists());
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

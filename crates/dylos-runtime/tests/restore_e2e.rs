#![allow(clippy::unwrap_used, clippy::expect_used)]

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

async fn progress(lab: &Lab, previous: u64) -> u64 {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let size = exec(lab, "A", "wc -c < /tmp/flow6.log")
                .await
                .parse::<u64>()
                .unwrap();
            if size > previous {
                return size;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("existing TCP flow stopped making progress")
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn restored_tcp_flows_keep_their_sockets_and_progress() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED restore_e2e: DYLOS_FC_BIN_DIR missing");
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
        eprintln!("SKIPPED restore_e2e: requires root, KVM and built images/binaries");
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
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    let mut original = Lab::up_with(&config, &spec, "src").await.unwrap();
    let mut restored = None;
    let result = std::panic::AssertUnwindSafe(async {
        exec(&original, "C", "iperf3 -6 -s -D -p 5201; iperf3 -4 -s -D -p 5202").await;
        exec(&original, "A", "iperf3 -6 -c fd64:796c:6f73:2::2 -p 5201 -t 0 -i 1 --forceflush > /tmp/flow6.log 2>&1 < /dev/null & true").await;
        exec(&original, "A", "iperf3 -4 -c 10.0.2.2 -p 5202 -t 0 -i 1 --forceflush > /tmp/flow4.log 2>&1 < /dev/null & true").await;
        progress(&original, 512).await;
        let sockets = exec(&original, "A", "ss -Htn state established | sort").await;
        assert!(sockets.contains("5201") && sockets.contains("5202"), "{sockets}");
        let manifest = original.snapshot("tcp").await.unwrap();
        let snapshot = config.store.snapshot_dir("src", "tcp");
        let clone = Lab::restore_with(&config, &snapshot, "dst").await.unwrap();
        restored = Some(clone);
        let clone = restored.as_ref().unwrap();
        assert_eq!(original.network().unwrap().plan().taps(), clone.network().unwrap().plan().taps());
        assert_eq!(original.network().unwrap().plan().bridges(), clone.network().unwrap().plan().bridges());
        assert_ne!(original.network().unwrap().netns_path(), clone.network().unwrap().netns_path());
        // Queue lengths vary; compare endpoints while requiring both pre-existing data connections.
        let endpoints = |text: &str| { let mut rows = text.lines().map(|line| line.split_whitespace().skip(2).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>(); rows.sort(); rows };
        assert_eq!(endpoints(&sockets), endpoints(&exec(clone, "A", "ss -Htn state established | sort").await));
        original.teardown().await.unwrap();
        let size = progress(clone, 512).await;
        progress(clone, size).await;
        let v4 = exec(clone, "A", "wc -c < /tmp/flow4.log").await.parse::<u64>().unwrap();
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if exec(clone, "A", "wc -c < /tmp/flow4.log").await.parse::<u64>().unwrap() > v4 { break; }
            }
        }).await.expect("IPv4 flow failed to progress");
        manifest.verify(dylos_store::SnapshotOpener::new(&snapshot).unwrap().verify_opener()).unwrap();
    }).catch_unwind().await;
    original.teardown().await.unwrap();
    if let Some(clone) = restored {
        let root = clone.root().to_owned();
        let paths: Vec<_> = clone
            .vms()
            .iter()
            .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
            .collect();
        let socket = clone.control_socket();
        let server = tokio::spawn(clone.supervise());
        Lab::request_teardown(&socket, Duration::from_secs(60))
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        assert!(!root.exists());
        for (paths, pid) in paths {
            assert!(!paths.jail_dir.exists());
            assert!(!paths.cgroup_dir.exists());
            assert!(!Path::new(&format!("/proc/{pid}")).exists());
        }
    }
    assert!(!config.netns_dir.join("dylos-src").exists());
    assert!(!config.netns_dir.join("dylos-dst").exists());
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

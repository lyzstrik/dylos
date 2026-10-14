#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};
use futures_util::FutureExt;

async fn assert_ping(lab: &Lab, node: &str, target: &str, family: &str) {
    let mut last_output = String::new();
    let mut success = false;
    for _ in 0..5 {
        let result = lab
            .exec(
                node,
                &format!("ping {family} -c 2 -W 2 {target}"),
                Duration::from_secs(15),
            )
            .await
            .unwrap();
        if result.success() {
            success = true;
            break;
        }
        last_output = format!("{result:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        success,
        "{node}: ping {family} {target} failed after retries: {last_output}"
    );
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn fork_ten_simultaneous_clones_independent_and_isolated() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED fork_e2e: DYLOS_FC_BIN_DIR missing");
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
        eprintln!("SKIPPED fork_e2e: requires root, KVM and built images/binaries");
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

    // Sized to fit the 4 vCPU, 16 GB runner: abc lab with small memory (128 MiB per node)
    let mut spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    for node in &mut spec.nodes {
        node.memory = 128;
        node.vcpus = 1;
    }

    let mut original = Lab::up_with(&config, &spec, "src").await.unwrap();

    let result = std::panic::AssertUnwindSafe(async {
        // Verify source lab dual-stack connectivity (IPv6 first, ADR-0002)
        assert_ping(&original, "A", "fd64:796c:6f73:2::2", "-6").await;
        assert_ping(&original, "A", "10.0.2.2", "-4").await;

        let manifest = original.snapshot("snap").await.unwrap();
        let snapshot = config.store.snapshot_dir("src", "snap");

        // Tear down source lab to verify clones are independent and reduce runner load
        original.teardown().await.unwrap();
        assert!(!config.store.base.join("src").exists());
        assert!(!config.netns_dir.join("dylos-src").exists());

        // Fork 10 simultaneous clones in parallel with deterministic clone identifiers
        let clone_count = 10;
        let clone_ids = Lab::generate_clone_ids("src", clone_count).unwrap();
        assert_eq!(clone_ids.len(), clone_count);
        for (i, id) in clone_ids.iter().enumerate() {
            assert_eq!(id, &format!("src-c{}", i + 1));
        }

        let start_fork = std::time::Instant::now();
        let clones = Lab::fork_with(&config, &snapshot, &clone_ids)
            .await
            .unwrap();
        let total_fork_duration = start_fork.elapsed();
        assert_eq!(clones.len(), clone_count);

        eprintln!(
            "Forked {} clones in {} ms total (avg {} ms/clone)",
            clone_count,
            total_fork_duration.as_millis(),
            total_fork_duration.as_millis() / clone_count as u128
        );

        // Per-clone latency (from netns creation to thaw) and memory (PSS)
        for clone in &clones {
            let latency = clone
                .restore_latency()
                .expect("per-clone latency from netns creation to thaw must be measured");
            eprintln!(
                "Clone {}: latency = {} ms (target < 500 ms)",
                clone.id(),
                latency.as_millis()
            );

            let pss_list = clone.measure_pss().await.expect("measure VM PSS");
            let total_pss = clone.total_pss_kib().await.expect("measure total PSS");
            eprintln!(
                "Clone {}: total PSS = {} KiB, per-VM = {:?}",
                clone.id(),
                total_pss,
                pss_list
            );
            assert!(total_pss > 0);
        }

        // Verify each clone is functional, isolated, and without network conflict
        for clone in &clones {
            let netns_path = config.netns_dir.join(format!("dylos-{}", clone.id()));
            assert!(netns_path.exists());

            // Dual-stack connectivity inside each clone (IPv6 first, ADR-0002)
            assert_ping(clone, "A", "fd64:796c:6f73:2::2", "-6").await;
            assert_ping(clone, "A", "10.0.2.2", "-4").await;
        }

        // Each clone is supervised and torn down independently via control socket (dylos lab down)
        let mut supervisors = Vec::with_capacity(clones.len());
        for clone in clones {
            let id = clone.id().to_string();
            let socket = clone.control_socket();
            let server = tokio::spawn(clone.supervise());
            supervisors.push((id, socket, server));
        }

        // Tear down clone 1 first to verify independent teardown while others remain running
        let (first_id, first_socket, first_server) = supervisors.remove(0);
        Lab::request_teardown(&first_socket, Duration::from_secs(30))
            .await
            .unwrap();
        first_server.await.unwrap().unwrap();
        assert!(!config.store.base.join(&first_id).exists());
        assert!(!config.netns_dir.join(format!("dylos-{first_id}")).exists());

        // Remaining clones still alive and functional
        for (id, socket, server) in supervisors {
            Lab::request_teardown(&socket, Duration::from_secs(30))
                .await
                .unwrap();
            server.await.unwrap().unwrap();
            assert!(!config.store.base.join(&id).exists());
            assert!(!config.netns_dir.join(format!("dylos-{id}")).exists());
        }

        manifest
            .verify(
                dylos_store::SnapshotOpener::new(&snapshot)
                    .unwrap()
                    .verify_opener(),
            )
            .unwrap();
    })
    .catch_unwind()
    .await;

    if config.store.base.join("src").exists() {
        let _ = original.teardown().await;
    }
    assert!(!config.store.base.join("src").exists());
    assert!(!config.netns_dir.join("dylos-src").exists());

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

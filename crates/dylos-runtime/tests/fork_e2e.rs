#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};
use futures_util::FutureExt;

async fn assert_dual_stack(lab: &Lab, from: &str, to_v6: &str, to_v4: &str, timeout: Duration) {
    // IPv6 first (ADR-0002)
    let start = std::time::Instant::now();
    let mut v6_ok = false;
    let mut last_v6_diag = String::new();
    while start.elapsed() < timeout {
        let res = lab
            .exec(
                from,
                &format!("ping -6 -c 1 -W 1 {to_v6}"),
                Duration::from_secs(5),
            )
            .await;
        match res {
            Ok(out) if out.success() => {
                v6_ok = true;
                break;
            }
            Ok(out) => {
                last_v6_diag = format!("code={}: {}", out.exit_code, out.stdout.trim());
            }
            Err(e) => {
                last_v6_diag = e.to_string();
            }
        }
    }
    if !v6_ok {
        let neigh = lab
            .exec(from, "ip -6 neigh show", Duration::from_secs(5))
            .await;
        let route = lab
            .exec(from, "ip -6 route show", Duration::from_secs(5))
            .await;
        assert!(
            v6_ok,
            "{from} -> {to_v6} (IPv6) ping timed out after {timeout:?}: {last_v6_diag}\nNeigh: {neigh:?}\nRoute: {route:?}"
        );
    }

    // IPv4 second
    let start = std::time::Instant::now();
    let mut v4_ok = false;
    let mut last_v4_diag = String::new();
    while start.elapsed() < timeout {
        let res = lab
            .exec(
                from,
                &format!("ping -4 -c 1 -W 1 {to_v4}"),
                Duration::from_secs(5),
            )
            .await;
        match res {
            Ok(out) if out.success() => {
                v4_ok = true;
                break;
            }
            Ok(out) => {
                last_v4_diag = format!("code={}: {}", out.exit_code, out.stdout.trim());
            }
            Err(e) => {
                last_v4_diag = e.to_string();
            }
        }
    }
    if !v4_ok {
        let neigh = lab
            .exec(from, "ip -4 neigh show", Duration::from_secs(5))
            .await;
        let route = lab
            .exec(from, "ip route show", Duration::from_secs(5))
            .await;
        assert!(
            v4_ok,
            "{from} -> {to_v4} (IPv4) ping timed out after {timeout:?}: {last_v4_diag}\nNeigh: {neigh:?}\nRoute: {route:?}"
        );
    }
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
        assert_dual_stack(
            &original,
            "A",
            "fd64:796c:6f73:2::2",
            "10.0.2.2",
            Duration::from_secs(15),
        )
        .await;

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
        let mut clones = Lab::fork_with(&config, &snapshot, &clone_ids)
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

        // Per-clone latency (from netns creation to thaw)
        for clone in &clones {
            let latency = clone
                .restore_latency()
                .expect("per-clone latency from netns creation to thaw must be measured");
            eprintln!(
                "Clone {}: latency = {} ms (target < 500 ms)",
                clone.id(),
                latency.as_millis()
            );
        }

        // 1. Verify memory sharing and read-only immutable backing across all clones
        for node in ["A", "B", "C"] {
            let c1_mem = clones[0].vm(node).unwrap().paths().root.join("vm.mem");
            let c1_meta = std::fs::metadata(&c1_mem).unwrap();
            let c1_ino = c1_meta.ino();
            assert_eq!(
                c1_meta.mode() & 0o777,
                0o444,
                "node {node} vm.mem must be read-only (0o444)"
            );
            assert_eq!(
                c1_meta.uid(),
                0,
                "node {node} vm.mem must be owned by root, not jailer UID"
            );

            for clone in &clones[1..] {
                let clone_mem = clone.vm(node).unwrap().paths().root.join("vm.mem");
                let clone_meta = std::fs::metadata(&clone_mem).unwrap();
                assert_eq!(
                    clone_meta.ino(),
                    c1_ino,
                    "clone {} node {} vm.mem must share the same backing inode",
                    clone.id(),
                    node
                );
                assert_eq!(
                    clone_meta.mode() & 0o777,
                    0o444,
                    "clone {} node {} vm.mem must be read-only",
                    clone.id(),
                    node
                );
                assert_eq!(
                    clone_meta.uid(),
                    0,
                    "clone {} node {} vm.mem must be root-owned",
                    clone.id(),
                    node
                );

                // Mutable VM state and rootfs must have distinct inodes
                let c1_state = clones[0].vm(node).unwrap().paths().root.join("vm.state");
                let clone_state = clone.vm(node).unwrap().paths().root.join("vm.state");
                assert_ne!(
                    std::fs::metadata(&c1_state).unwrap().ino(),
                    std::fs::metadata(&clone_state).unwrap().ino(),
                    "vm.state must have distinct inodes across clones"
                );
            }
        }

        // 2. Verify distinct network namespaces for all clones
        let mut netns_inodes = HashSet::new();
        for clone in &clones {
            let netns_path = config.netns_dir.join(format!("dylos-{}", clone.id()));
            assert!(netns_path.exists());
            let ino = std::fs::metadata(&netns_path).unwrap().ino();
            assert!(
                netns_inodes.insert(ino),
                "clone {} netns inode {} is not unique",
                clone.id(),
                ino
            );
        }

        // 3. Isolation: negative reachability from host outside netns
        let host_ping_v6 = tokio::process::Command::new("ping")
            .args(["-6", "-c", "1", "-W", "1", "fd64:796c:6f73:1::2"])
            .output()
            .await
            .unwrap();
        assert!(
            !host_ping_v6.status.success(),
            "Host outside netns must not reach clone IPv6"
        );
        let host_ping_v4 = tokio::process::Command::new("ping")
            .args(["-4", "-c", "1", "-W", "1", "10.0.1.2"])
            .output()
            .await
            .unwrap();
        assert!(
            !host_ping_v4.status.success(),
            "Host outside netns must not reach clone IPv4"
        );

        // 4. Positive host-originated reachability after entering clone 1 netns
        let c1_netns = config.netns_dir.join(format!("dylos-{}", clones[0].id()));
        let netns_arg = format!("--net={}", c1_netns.display());
        let add_v6 = tokio::process::Command::new("nsenter")
            .args([
                &netns_arg,
                "ip",
                "-6",
                "addr",
                "add",
                "fd64:796c:6f73:1::fe/64",
                "dev",
                "br-left",
                "nodad",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            add_v6.status.success(),
            "failed to add v6 addr inside c1 netns: {add_v6:?}"
        );
        let add_v4 = tokio::process::Command::new("nsenter")
            .args([
                &netns_arg,
                "ip",
                "-4",
                "addr",
                "add",
                "10.0.1.254/24",
                "dev",
                "br-left",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            add_v4.status.success(),
            "failed to add v4 addr inside c1 netns: {add_v4:?}"
        );

        let start_ns = std::time::Instant::now();
        let mut ns_v6_ok = false;
        let mut last_ns_v6 = String::new();
        while start_ns.elapsed() < Duration::from_secs(10) {
            let res = tokio::process::Command::new("nsenter")
                .args([
                    &netns_arg,
                    "ping",
                    "-6",
                    "-c",
                    "1",
                    "-W",
                    "1",
                    "-I",
                    "br-left",
                    "fd64:796c:6f73:1::2",
                ])
                .output()
                .await
                .unwrap();
            if res.status.success() {
                ns_v6_ok = true;
                break;
            }
            last_ns_v6 = format!("code={:?}: {}", res.status.code(), String::from_utf8_lossy(&res.stderr));
        }
        assert!(
            ns_v6_ok,
            "Host entered c1 netns must reach node A over IPv6: {last_ns_v6}"
        );

        let start_ns = std::time::Instant::now();
        let mut ns_v4_ok = false;
        let mut last_ns_v4 = String::new();
        while start_ns.elapsed() < Duration::from_secs(10) {
            let res = tokio::process::Command::new("nsenter")
                .args([
                    &netns_arg,
                    "ping",
                    "-4",
                    "-c",
                    "1",
                    "-W",
                    "1",
                    "-I",
                    "br-left",
                    "10.0.1.2",
                ])
                .output()
                .await
                .unwrap();
            if res.status.success() {
                ns_v4_ok = true;
                break;
            }
            last_ns_v4 = format!("code={:?}: {}", res.status.code(), String::from_utf8_lossy(&res.stderr));
        }
        assert!(
            ns_v4_ok,
            "Host entered c1 netns must reach node A over IPv4: {last_ns_v4}"
        );

        // 5. Measure PSS under N=10 for clone 1
        let pss_n10 = clones[0]
            .total_pss_kib()
            .await
            .expect("measure total PSS for clone 1 under N=10");
        eprintln!("PSS under N=10: clone 1 total PSS = {pss_n10} KiB");
        assert!(pss_n10 > 0);

        // 6. Dual-stack connectivity inside each clone (IPv6 first, ADR-0002)
        for clone in &clones {
            assert_dual_stack(
                clone,
                "A",
                "fd64:796c:6f73:2::2",
                "10.0.2.2",
                Duration::from_secs(15),
            )
            .await;
        }

        // Bidirectional flow (C -> A) and router-initiated cold neighbor (B -> A)
        assert_dual_stack(
            &clones[0],
            "C",
            "fd64:796c:6f73:1::2",
            "10.0.1.2",
            Duration::from_secs(15),
        )
        .await;
        assert_dual_stack(
            &clones[0],
            "B",
            "fd64:796c:6f73:1::2",
            "10.0.1.2",
            Duration::from_secs(15),
        )
        .await;

        // 7. Tear down clone 1 first via control socket to verify independent teardown
        let c1 = clones.remove(0);
        let c1_id = c1.id().to_string();
        let c1_socket = c1.control_socket();
        let c1_server = tokio::spawn(c1.supervise());
        Lab::request_teardown(&c1_socket, Duration::from_secs(30))
            .await
            .unwrap();
        c1_server.await.unwrap().unwrap();
        assert!(!config.store.base.join(&c1_id).exists());
        assert!(!config.netns_dir.join(format!("dylos-{c1_id}")).exists());

        // 8. Exercise surviving clones to assert they still work after c1 was downed
        assert_dual_stack(
            &clones[0],
            "A",
            "fd64:796c:6f73:2::2",
            "10.0.2.2",
            Duration::from_secs(15),
        )
        .await;
        assert_dual_stack(
            &clones[clones.len() - 1],
            "A",
            "fd64:796c:6f73:2::2",
            "10.0.2.2",
            Duration::from_secs(15),
        )
        .await;

        // 9. Tear down clones 2 through 9, leaving only the last clone running
        while clones.len() > 1 {
            let c = clones.remove(0);
            let id = c.id().to_string();
            let socket = c.control_socket();
            let server = tokio::spawn(c.supervise());
            Lab::request_teardown(&socket, Duration::from_secs(30))
                .await
                .unwrap();
            server.await.unwrap().unwrap();
            assert!(!config.store.base.join(&id).exists());
            assert!(!config.netns_dir.join(format!("dylos-{id}")).exists());
        }

        // 10. Measure PSS under N=1 for the single surviving clone and compare
        let last_clone = clones.remove(0);
        let pss_n1 = last_clone
            .total_pss_kib()
            .await
            .expect("measure total PSS for clone 10 under N=1");
        eprintln!(
            "PSS comparison: N=10 per-clone PSS = {} KiB, N=1 clone PSS = {} KiB (ratio: {:.2}x)",
            pss_n10,
            pss_n1,
            pss_n1 as f64 / pss_n10.max(1) as f64
        );
        assert!(
            pss_n1 >= pss_n10,
            "N=1 PSS ({pss_n1} KiB) should be at least N=10 PSS ({pss_n10} KiB) due to memory sharing"
        );

        // Exercise the lone clone once more
        assert_dual_stack(
            &last_clone,
            "A",
            "fd64:796c:6f73:2::2",
            "10.0.2.2",
            Duration::from_secs(15),
        )
        .await;

        // 11. Tear down the last clone
        let last_id = last_clone.id().to_string();
        let last_socket = last_clone.control_socket();
        let last_server = tokio::spawn(last_clone.supervise());
        Lab::request_teardown(&last_socket, Duration::from_secs(30))
            .await
            .unwrap();
        last_server.await.unwrap().unwrap();
        assert!(!config.store.base.join(&last_id).exists());
        assert!(!config.netns_dir.join(format!("dylos-{last_id}")).exists());

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

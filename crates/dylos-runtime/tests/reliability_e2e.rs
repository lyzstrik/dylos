#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};
use futures_util::FutureExt;

fn configuration() -> Option<(tempfile::TempDir, LabConfig, LabSpec)> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bin = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from);
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");
    let Some(bin) = bin else {
        eprintln!("SKIPPED reliability_e2e: DYLOS_FC_BIN_DIR missing");
        return None;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0
        || [
            Path::new("/dev/kvm"),
            &kernel,
            &image,
            &bin.join("jailer"),
            &bin.join("firecracker"),
        ]
        .iter()
        .any(|path| !path.exists())
    {
        eprintln!("SKIPPED reliability_e2e: requires root, KVM, images and binaries");
        return None;
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
    Some((dir, config, spec))
}

async fn exec(lab: &Lab, node: &str, command: &str) -> String {
    let result = lab
        .exec(node, command, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        result.success(),
        "{} {node}: {command}: {result:?}",
        lab.id()
    );
    result.stdout
}

async fn ping(lab: &Lab) {
    for node in ["A", "B"] {
        exec(lab, node, "ip -6 neigh flush all; ip -4 neigh flush all").await;
    }
    exec(lab, "A", "ping -6 -c 1 -W 5 fd64:796c:6f73:2::2").await;
    exec(lab, "A", "ping -4 -c 1 -W 5 10.0.2.2").await;
}

async fn endpoints(lab: &Lab, node: &str) -> Vec<String> {
    let text = exec(lab, node, "ss -Htn state established").await;
    let mut rows = text
        .lines()
        .map(|line| {
            line.split_whitespace()
                .skip(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

async fn received(lab: &Lab, family: &str) -> u64 {
    let text = exec(lab, "C", &format!("ss {family} -tin state established")).await;
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix("bytes_received:"))
        .map(|value| value.parse::<u64>().unwrap())
        .sum()
}

async fn progress(lab: &Lab, family: &str, previous: u64) -> u64 {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let count = received(lab, family).await;
            if count > previous + 65536 {
                return count;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("captured TCP connection stopped receiving bytes")
}

async fn down(mut lab: Lab) {
    let root = lab.root().to_owned();
    let namespace = lab.network().unwrap().netns_path().to_owned();
    let namespace_handle = std::fs::File::open(&namespace).unwrap();
    let paths: Vec<_> = lab
        .vms()
        .iter()
        .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
        .collect();
    lab.teardown().await.unwrap();
    lab.teardown().await.unwrap();
    assert!(!root.exists());
    assert!(!namespace.exists());
    // Keep a handle after unmount to inspect the actual namespace, including
    // persistent TAPs and bridges that a missing mount path would conceal.
    let output = tokio::process::Command::new("nsenter")
        .arg(format!(
            "--net=/proc/{}/fd/{}",
            std::process::id(),
            namespace_handle.as_raw_fd()
        ))
        .args(["ip", "-j", "link", "show"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let links: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        links.iter().filter(|link| link["ifname"] != "lo").count(),
        0,
        "{links:?}"
    );
    drop(namespace_handle);
    for (paths, pid) in paths {
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!paths.jail_dir.exists());
        assert!(!paths.cgroup_dir.exists());
    }
}

fn empty_directory(path: &Path) {
    if path.exists() {
        assert_eq!(
            std::fs::read_dir(path).unwrap().count(),
            0,
            "{}",
            path.display()
        );
    }
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn tcp_survives_in_all_ten_clones() {
    let Some((_dir, config, spec)) = configuration() else {
        return;
    };
    let mut source = Lab::up_with(&config, &spec, "survival").await.unwrap();
    let mut clones = Vec::new();
    let result = std::panic::AssertUnwindSafe(async {
        exec(&source, "C", "iperf3 -6 -s -D -p 5201; iperf3 -4 -s -D -p 5202").await;
        exec(&source, "A", include_str!("fixtures/reliability-flows.sh")).await;
        progress(&source, "-6", 65536).await;
        progress(&source, "-4", 65536).await;
        let a = endpoints(&source, "A").await;
        let c = endpoints(&source, "C").await;
        assert_eq!(a.len(), 4, "two control and two data sockets: {a:?}");
        source.snapshot("tcp").await.unwrap();
        let ids = Lab::generate_clone_ids("survival", 10).unwrap();
        clones = Lab::fork_with(&config, &config.store.snapshot_dir("survival", "tcp"), &ids).await.unwrap();
        assert_eq!(clones.len(), 10);
        source.teardown().await.unwrap();
        let namespaces: std::collections::HashSet<_> = clones.iter()
            .map(|lab| lab.network().unwrap().netns_path()).collect();
        assert_eq!(namespaces.len(), 10);
        for clone in &clones {
            for family in ["-6", "-4"] {
                let before = received(clone, family).await;
                progress(clone, family, before).await;
            }
            assert_eq!(endpoints(clone, "A").await, a);
            assert_eq!(endpoints(clone, "C").await, c);
            ping(clone).await;
        }
        eprintln!("LYZ-27 survival: 10/10 clones, existing IPv6/IPv4 TCP sockets advancing; dual-stack ping passed");
    }).catch_unwind().await;
    for clone in clones {
        down(clone).await;
    }
    source.teardown().await.unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn snapshot_fork_down_endurance() {
    let cycles = std::env::var("DYLOS_ENDURANCE_CYCLES").map_or(100, |value| {
        value
            .parse::<usize>()
            .expect("DYLOS_ENDURANCE_CYCLES must be a positive integer")
    });
    assert!(cycles > 0);
    let Some((_dir, config, spec)) = configuration() else {
        return;
    };
    let started = Instant::now();
    for cycle in 0..cycles {
        let mut source = Lab::up_with(&config, &spec, "endurance").await.unwrap();
        let mut clones = Vec::new();
        let result = std::panic::AssertUnwindSafe(async {
            source.snapshot("cycle").await.unwrap();
            let ids = Lab::generate_clone_ids("endurance", 3).unwrap();
            clones = Lab::fork_with(
                &config,
                &config.store.snapshot_dir("endurance", "cycle"),
                &ids,
            )
            .await
            .unwrap();
            assert_eq!(clones.len(), 3);
            for clone in &clones {
                ping(clone).await;
            }
        })
        .catch_unwind()
        .await;
        for clone in clones {
            down(clone).await;
        }
        down(source).await;
        empty_directory(&config.store.base);
        empty_directory(&config.netns_dir);
        eprintln!(
            "LYZ-27 endurance: cycle {}/{cycles}, netns/TAPs/bridges/processes/files remaining=0",
            cycle + 1
        );
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
    eprintln!(
        "LYZ-27 endurance: {cycles}/{cycles} passed in {} ms",
        started.elapsed().as_millis()
    );
}

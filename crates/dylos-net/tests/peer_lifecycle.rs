#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use dylos_core::LabSpec;
use dylos_net::{FabricPlan, LabNetwork, teardown};

fn sandbox(test: &str) -> bool {
    if std::env::var_os("DYLOS_PEER_SANDBOX").is_some() {
        return true;
    }
    if !Command::new("unshare")
        .args(["-Urmn", "true"])
        .status()
        .unwrap()
        .success()
    {
        eprintln!("SKIPPED {test}: unprivileged user namespaces unavailable");
        return false;
    }
    let binary_dir = tempfile::tempdir().unwrap();
    let binary = binary_dir.path().join("peer-lifecycle");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    assert!(
        Command::new("unshare")
            .arg("-Urmn")
            .arg(&binary)
            .args([test, "--exact", "--nocapture"])
            .env("DYLOS_PEER_SANDBOX", "1")
            .status()
            .unwrap()
            .success()
    );
    false
}

fn rules(path: &Path) -> String {
    let output = Command::new("nsenter")
        .arg(format!("--net={}", path.display()))
        .args(["nft", "list", "table", "bridge", "dylos_peer_filter"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn policy_survives_freeze_and_clone_and_teardown_is_idempotent() {
    if !sandbox("policy_survives_freeze_and_clone_and_teardown_is_idempotent") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let spec = LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap();
        let first = FabricPlan::new("first", &spec).unwrap();
        let second = FabricPlan::new("second", &spec).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut original = LabNetwork::create(first.clone(), dir.path(), 0, 0)
            .await
            .unwrap();
        let mut clone = LabNetwork::create_frozen(second, dir.path(), 0, 0)
            .await
            .unwrap();
        let before = rules(original.netns_path());
        assert_eq!(before, rules(clone.netns_path()));
        original.freeze().await.unwrap();
        assert_eq!(before, rules(original.netns_path()));
        original.thaw().await.unwrap();
        clone.thaw().await.unwrap();
        assert_eq!(before, rules(original.netns_path()));
        // Retain the namespace to prove explicit table cleanup, rather than kernel destruction.
        let namespace = std::fs::File::open(original.netns_path()).unwrap();
        original.teardown().await.unwrap();
        original.teardown().await.unwrap();
        teardown(&first, dir.path()).await.unwrap();
        let output = Command::new("nsenter")
            .arg(format!(
                "--net=/proc/{}/fd/{}",
                std::process::id(),
                std::os::fd::AsRawFd::as_raw_fd(&namespace)
            ))
            .args(["nft", "list", "tables"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, Vec::<u8>::new());
        assert_eq!(before, rules(clone.netns_path()));
        clone.teardown().await.unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    });
}

#[test]
#[ignore = "requires nft and user/network namespaces; exercised by just e2e"]
fn peer_filter_lifecycle_e2e() {
    policy_survives_freeze_and_clone_and_teardown_is_idempotent();
}

fn packets(path: &Path, mode: &str) {
    let mut child = Command::new("nsenter")
        .arg(format!("--net={}", path.display()))
        .args(["python3", "-", mode])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_str!("fixtures/peer-packets.py").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn packet_guards_and_bypasses_survive_freeze_and_clone() {
    if !sandbox("packet_guards_and_bypasses_survive_freeze_and_clone") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut spec = LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut original = LabNetwork::create(
            FabricPlan::new("original", &spec).unwrap(),
            dir.path(),
            0,
            0,
        )
        .await
        .unwrap();
        let mut clone =
            LabNetwork::create_frozen(FabricPlan::new("clone", &spec).unwrap(), dir.path(), 0, 0)
                .await
                .unwrap();
        packets(original.netns_path(), "filtered");
        packets(clone.netns_path(), "frozen");
        original.freeze().await.unwrap();
        packets(original.netns_path(), "frozen");
        original.thaw().await.unwrap();
        clone.thaw().await.unwrap();
        packets(original.netns_path(), "filtered");
        packets(clone.netns_path(), "filtered");
        let old_plan = original.plan().clone();
        teardown(&old_plan, dir.path()).await.unwrap();
        let mut replacement = LabNetwork::create(old_plan, dir.path(), 0, 0)
            .await
            .unwrap();
        original.teardown().await.unwrap();
        packets(replacement.netns_path(), "filtered");
        replacement.teardown().await.unwrap();
        clone.teardown().await.unwrap();
        spec.peer_filtering = false;
        let mut unfiltered = LabNetwork::create(
            FabricPlan::new("unfiltered", &spec).unwrap(),
            dir.path(),
            0,
            0,
        )
        .await
        .unwrap();
        packets(unfiltered.netns_path(), "unfiltered");
        unfiltered.teardown().await.unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    });
}

#[test]
#[ignore = "requires nft and user/network namespaces; exercised by just e2e"]
fn peer_filter_packets_e2e() {
    packet_guards_and_bypasses_survive_freeze_and_clone();
    cancelled_filtered_creation_rolls_back();
    unspecified_dad_is_rejected_even_if_declared_in_the_spec();
    measure_peer_filter_install_overhead();
}

#[test]
fn rejected_executable_or_failed_atomic_install_leaves_no_fabric() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !sandbox("rejected_executable_or_failed_atomic_install_leaves_no_fabric") {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let executable = fixture.path().join("nft");
    std::fs::write(&executable, include_str!("fixtures/nft-install-failure.sh")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o555)).unwrap();
    for candidate in ["/usr/sbin/nft", "/usr/bin/nft"] {
        if Path::new(candidate).exists() {
            assert!(
                Command::new("mount")
                    .arg("--bind")
                    .arg(&executable)
                    .arg(candidate)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let spec = LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap();
        let plan = FabricPlan::new("failure", &spec).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let error = LabNetwork::create(plan, dir.path(), 0, 0)
            .await
            .unwrap_err()
            .to_string();
        if std::fs::metadata("/").unwrap().uid() == std::fs::metadata(&executable).unwrap().uid() {
            assert!(error.contains("exit status: 1"), "{error}");
        } else {
            assert!(error.contains("filesystem root owner"), "{error}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    });
}

#[test]
#[ignore = "requires nft and user/network namespaces; exercised by just e2e"]
fn peer_filter_install_failure_e2e() {
    rejected_executable_or_failed_atomic_install_leaves_no_fabric();
}

#[test]
fn cancelled_filtered_creation_rolls_back() {
    if !sandbox("cancelled_filtered_creation_rolls_back") {
        return;
    }
    let spec = LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap();
    let plan = FabricPlan::new("cancelled", &spec).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(plan.netns_name());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut creation = Box::pin(LabNetwork::create(plan, dir.path(), 0, 0));
        std::future::poll_fn(|context| {
            let _ = creation.as_mut().poll(context);
            std::task::Poll::Ready(())
        })
        .await;
    });
    drop(runtime);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "cancelled namespace retained"
        );
        std::thread::yield_now();
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn unspecified_dad_is_rejected_even_if_declared_in_the_spec() {
    if !sandbox("unspecified_dad_is_rejected_even_if_declared_in_the_spec") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut spec = LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap();
        spec.segments[0].ipv6 = "::/0".parse().unwrap();
        spec.nodes[0].interfaces[0].ipv6 = "::/128".parse().unwrap();
        spec.validate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut net = LabNetwork::create(FabricPlan::new("dad", &spec).unwrap(), dir.path(), 0, 0)
            .await
            .unwrap();
        packets(net.netns_path(), "unspecified");
        net.teardown().await.unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    });
}

#[test]
fn measure_peer_filter_install_overhead() {
    if !sandbox("measure_peer_filter_install_overhead") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let spec = LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut installs = Vec::new();
        let mut fabrics = Vec::new();
        for index in 0..20 {
            let plan = FabricPlan::new(&format!("measure{index}"), &spec).unwrap();
            let rendered = plan.peer_filter_rules().unwrap();
            let started = std::time::Instant::now();
            let mut net = LabNetwork::create_frozen(plan, dir.path(), 0, 0)
                .await
                .unwrap();
            fabrics.push(started.elapsed().as_secs_f64() * 1000.0);
            let started = std::time::Instant::now();
            let mut child = Command::new("nsenter")
                .arg(format!("--net={}", net.netns_path().display()))
                .args(["nft", "-f", "-"])
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let batch = format!("delete table bridge dylos_peer_filter\n{rendered}");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(batch.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            installs.push(started.elapsed().as_secs_f64() * 1000.0);
            net.teardown().await.unwrap();
        }
        installs.sort_by(f64::total_cmp);
        fabrics.sort_by(f64::total_cmp);
        eprintln!(
            "install_with_nsenter_ms p50={:.3} p95={:.3} samples=20",
            installs[10], installs[18]
        );
        eprintln!(
            "filtered_frozen_fabric_ms p50={:.3} p95={:.3} samples=20",
            fabrics[10], fabrics[18]
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    });
}

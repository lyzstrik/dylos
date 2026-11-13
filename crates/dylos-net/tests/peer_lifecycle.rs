#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

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

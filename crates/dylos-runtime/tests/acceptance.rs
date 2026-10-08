#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use dylos_fc::config::{ActionType, InstanceActionInfo};
use dylos_runtime::jailer::ChrootFile;
use dylos_runtime::{ProcessState, Timeouts, Vm, VmSpec};
use support::{Mode, Sandbox};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

fn spec(sandbox: &Sandbox) -> VmSpec {
    let kernel = sandbox.dir.path().join("vmlinux.bin");
    std::fs::write(&kernel, b"kernel").unwrap();
    VmSpec {
        lab_id: "acpt-lab".into(),
        node: "node1".into(),
        netns: None,
        files: vec![ChrootFile::new(kernel, "vmlinux.bin")],
        timeouts: Timeouts {
            ready: Duration::from_secs(1),
            graceful: Duration::from_millis(100),
            term: Duration::from_millis(100),
            kill: Duration::from_millis(100),
            metrics_poll: Duration::from_millis(50),
        },
    }
}

async fn assert_no_leftovers(sandbox: &Sandbox) {
    let my_pid = std::process::id();
    let mut dir = tokio::fs::read_dir("/proc").await.unwrap();
    let mut left_procs = Vec::new();
    while let Some(entry) = dir.next_entry().await.unwrap() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_ok()
            && let Ok(stat) = tokio::fs::read_to_string(entry.path().join("stat")).await
        {
            let parts: Vec<&str> = stat.split_whitespace().collect();
            if parts.len() > 3 && parts[3] == my_pid.to_string() {
                let name = parts
                    .get(1)
                    .map_or("unknown", |s| s.trim_matches(|c| c == '(' || c == ')'));
                left_procs.push(format!("pid {} ({}) state {}", parts[0], name, parts[2]));
            }
        }
    }
    assert!(
        left_procs.is_empty(),
        "Leftover child processes: {left_procs:#?}"
    );
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn shutdown_after_failed_start() {
    let sandbox = Sandbox::new(Mode::NoSocket); // Causes ReadyTimeout
    let result = Vm::launch(&sandbox.config, &spec(&sandbox)).await;
    assert!(result.is_err(), "Expected launch to fail due to timeout");

    // Check that cleanup was successful and no resources are left
    assert_no_leftovers(&sandbox).await;
}

#[tokio::test]
async fn double_shutdown() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();

    vm.shutdown().await.unwrap();
    vm.shutdown().await.unwrap(); // Idempotent

    assert_no_leftovers(&sandbox).await;
}

#[tokio::test]
async fn unexpected_death() {
    let sandbox = Sandbox::new(Mode::CrashOnFlush);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();

    // Trigger unexpected death
    let flush = InstanceActionInfo::new(ActionType::FlushMetrics);
    let _: Option<serde_json::Value> = vm.client().put("/actions", &flush).await.unwrap_or(None);

    let mut state = vm.state();
    tokio::time::timeout(
        Duration::from_secs(2),
        state.wait_for(|s| *s != ProcessState::Running),
    )
    .await
    .unwrap()
    .unwrap();

    vm.shutdown().await.unwrap(); // Cleanup

    assert_no_leftovers(&sandbox).await;
}

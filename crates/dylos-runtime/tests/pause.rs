#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use dylos_runtime::{LabError, Timeouts, Vm, VmSpec, pause_all, resume_all};
use support::{Captured, Mode, Sandbox};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

async fn setup() -> (Sandbox, Vec<Vm>) {
    let sandbox = Sandbox::new(Mode::ParallelPause);
    let mut vms = Vec::new();
    for node in ["A", "B", "C"] {
        vms.push(
            Vm::launch(
                &sandbox.config,
                &VmSpec {
                    lab_id: "lab".into(),
                    node: node.into(),
                    vcpu_count: 1,
                    mem_size_mib: 128,
                    netns: None,
                    files: vec![],
                    timeouts: Timeouts::default(),
                },
            )
            .await
            .unwrap(),
        );
    }
    (sandbox, vms)
}

#[tokio::test]
async fn pause_and_resume_dispatch_every_request_before_waiting_and_log_skew() {
    let (sandbox, mut vms) = setup().await;
    let (logs, _guard) = Captured::install();
    pause_all("lab", &vms).await.unwrap();
    assert!(logs.contains("skew_us"));
    resume_all("lab", &vms).await.unwrap();
    for vm in &vms {
        for action in ["paused", "resumed"] {
            assert!(
                sandbox
                    .dir
                    .path()
                    .join(format!("{}.{action}-barrier", vm.paths().id))
                    .exists(),
                "every fake must pass the all-requests barrier before answering"
            );
        }
    }
    for vm in &mut vms {
        let pid = vm.pid().unwrap();
        let paths = vm.paths().clone();
        vm.shutdown().await.unwrap();
        vm.shutdown().await.unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(!paths.api_socket().exists());
    }
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn failed_pause_resumes_every_vm_and_preserves_rollback_errors() {
    for (pause_failure, rollback_fails) in [
        ("fail-paused", false),
        ("fail-paused", true),
        ("drop-paused", false),
        ("drop-paused", true),
    ] {
        let (sandbox, mut vms) = setup().await;
        let failed = vms[0].paths().root.clone();
        std::fs::write(failed.join(pause_failure), b"").unwrap();
        if rollback_fails {
            std::fs::write(vms[1].paths().root.join("fail-resumed"), b"").unwrap();
        }
        let (logs, _guard) = Captured::install();
        let error = pause_all("lab", &vms).await.unwrap_err();
        assert!(!logs.contains("skew_us"));
        assert_eq!(matches!(error, LabError::Rollback { .. }), rollback_fails);
        assert!(error.to_string().contains("lab lab, VM"));
        for (index, vm) in vms.iter().enumerate() {
            assert!(
                sandbox
                    .dir
                    .path()
                    .join(format!("{}.resumed", vm.paths().id))
                    .exists()
            );
            assert_eq!(
                vm.paths().root.join("is-paused").exists(),
                rollback_fails && index == 1
            );
        }
        for vm in &mut vms {
            let pid = vm.pid().unwrap();
            let paths = vm.paths().clone();
            vm.shutdown().await.unwrap();
            vm.shutdown().await.unwrap();
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
            assert!(!paths.api_socket().exists());
        }
        sandbox.assert_no_jail_left();
    }
}

#[tokio::test]
async fn resume_failure_still_waits_for_every_vm() {
    let (sandbox, mut vms) = setup().await;
    std::fs::write(vms[0].paths().root.join("fail-resumed"), b"").unwrap();
    assert!(resume_all("lab", &vms).await.is_err());
    for vm in &vms {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
    }
    for vm in &mut vms {
        let pid = vm.pid().unwrap();
        let paths = vm.paths().clone();
        vm.shutdown().await.unwrap();
        vm.shutdown().await.unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(!paths.api_socket().exists());
    }
    sandbox.assert_no_jail_left();
}

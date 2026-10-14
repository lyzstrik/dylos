#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use dylos_runtime::{Lab, LabError, Timeouts, Vm, VmSpec, pause_all, resume_all};
use std::time::Duration;
use support::{Captured, Mode, Sandbox};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

async fn setup() -> (Sandbox, Lab) {
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
    (sandbox, Lab::from_test_vms("lab", vms))
}

#[tokio::test]
async fn pause_and_resume_dispatch_every_request_before_waiting_and_log_skew() {
    let (sandbox, mut lab) = setup().await;
    let (logs, _guard) = Captured::install();
    let skew = pause_all(&mut lab).await.unwrap();
    assert!(skew < Duration::from_secs(3));
    assert!(logs.contains("skew_us"));
    resume_all(&mut lab).await.unwrap();
    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn failed_pause_resumes_all_confirmed_vms_and_preserves_rollback_errors() {
    for rollback_fails in [false, true] {
        let (sandbox, mut lab) = setup().await;
        let failed = lab.vms()[0].paths().root.clone();
        std::fs::write(failed.join("fail-paused"), b"").unwrap();
        if rollback_fails {
            std::fs::write(lab.vms()[1].paths().root.join("fail-resumed"), b"").unwrap();
        }
        let error = pause_all(&mut lab).await.unwrap_err();
        assert_eq!(matches!(error, LabError::Rollback { .. }), rollback_fails);
        assert!(error.to_string().contains("lab lab, VM"));
        for (index, vm) in lab.vms().iter().enumerate() {
            assert_eq!(
                sandbox
                    .dir
                    .path()
                    .join(format!("{}.resumed", vm.paths().id))
                    .exists(),
                index != 0
            );
        }
        lab.teardown().await.unwrap();
        sandbox.assert_no_jail_left();
    }
}

#[tokio::test]
async fn resume_failure_still_waits_for_every_vm() {
    let (sandbox, mut lab) = setup().await;
    std::fs::write(lab.vms()[0].paths().root.join("fail-resumed"), b"").unwrap();
    assert!(resume_all(&mut lab).await.is_err());
    for vm in lab.vms() {
        assert!(
            sandbox
                .dir
                .path()
                .join(format!("{}.resumed", vm.paths().id))
                .exists()
        );
    }
    lab.teardown().await.unwrap();
    sandbox.assert_no_jail_left();
}

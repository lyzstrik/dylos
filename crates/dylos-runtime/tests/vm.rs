mod support;

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use dylos_fc::config::{ActionType, InstanceActionInfo};
use dylos_runtime::jailer::ChrootFile;
use dylos_runtime::{Error, ProcessState, Timeouts, Vm, VmSpec};
use support::{Captured, Mode, Sandbox, wait_until};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

#[allow(clippy::unwrap_used)]
fn spec(sandbox: &Sandbox) -> VmSpec {
    let kernel = sandbox.dir.path().join("vmlinux.bin");
    std::fs::write(&kernel, b"kernel").unwrap();
    VmSpec {
        vcpu_count: 1,
        mem_size_mib: 128,
        lab_id: "lab1".into(),
        node: "web".into(),
        netns: None,
        files: vec![ChrootFile::new(kernel, "vmlinux.bin")],
        timeouts: Timeouts {
            ready: Duration::from_secs(5),
            graceful: Duration::from_millis(500),
            term: Duration::from_millis(500),
            kill: Duration::from_secs(5),
            metrics_poll: Duration::from_millis(50),
        },
    }
}

#[allow(clippy::expect_used)]
async fn exit_state(vm: &Vm) -> ProcessState {
    let mut state = vm.state();
    tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(|s| *s != ProcessState::Running),
    )
    .await
    .expect("process did not exit")
    .expect("supervisor gone")
    .clone()
}

/// Raw wait statuses: exit code in the second byte, terminating signal in the low bits.
fn exited(code: i32) -> ProcessState {
    ProcessState::Exited(ExitStatus::from_raw(code << 8))
}

fn killed_by(signal: i32) -> ProcessState {
    ProcessState::Exited(ExitStatus::from_raw(signal))
}

#[tokio::test]
async fn launch_waits_for_api_and_shutdown_is_graceful_and_idempotent() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let root = vm.paths().root.clone();
    assert!(root.join("vmlinux.bin").exists());
    assert!(vm.paths().api_socket().exists());
    assert!(vm.paths().cgroup_dir.is_dir());

    vm.shutdown().await.unwrap();
    assert_eq!(exit_state(&vm).await, exited(0));
    sandbox.assert_no_jail_left();
    vm.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn shutdown_escalates_to_sigterm() {
    let sandbox = Sandbox::new(Mode::IgnoreCtrlAltDel);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    vm.shutdown().await.unwrap();
    assert_eq!(exit_state(&vm).await, killed_by(15));
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn shutdown_escalates_to_sigkill() {
    let sandbox = Sandbox::new(Mode::IgnoreTerm);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    vm.shutdown().await.unwrap();
    assert_eq!(exit_state(&vm).await, killed_by(9));
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn unexpected_death_is_reported_and_metrics_and_logs_reach_tracing() {
    let (logs, _guard) = Captured::install();
    let sandbox = Sandbox::new(Mode::CrashOnFlush);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let flush = InstanceActionInfo::new(ActionType::FlushMetrics);
    let _: Option<serde_json::Value> = vm.client().put("/actions", &flush).await.unwrap();

    wait_until("metrics are captured", Duration::from_secs(5), || {
        logs.contains("process_startup_time_us")
    })
    .await;
    assert_eq!(exit_state(&vm).await, exited(7));
    vm.shutdown().await.unwrap();
    assert!(logs.contains("Firecracker exited unexpectedly"));
    wait_until("metrics are captured", Duration::from_secs(5), || {
        logs.contains("process_startup_time_us")
    })
    .await;
    assert!(logs.contains("fake firecracker starting"));
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn ready_timeout_kills_and_cleans_up() {
    let sandbox = Sandbox::new(Mode::NoSocket);
    let mut spec = spec(&sandbox);
    spec.timeouts.ready = Duration::from_millis(300);
    let err = Vm::launch(&sandbox.config, &spec).await.err().unwrap();
    assert!(matches!(err, Error::ReadyTimeout { .. }), "{err}");
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn exit_during_start_is_reported_and_cleaned_up() {
    let sandbox = Sandbox::new(Mode::ExitAtOnce);
    let err = Vm::launch(&sandbox.config, &spec(&sandbox))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, Error::ExitedDuringStart { state, .. } if *state == exited(3)),
        "{err}"
    );
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn launcher_that_cannot_spawn_leaves_nothing() {
    let mut sandbox = Sandbox::new(Mode::Serve);
    sandbox.config.jailer = sandbox.dir.path().join("missing-jailer");
    let err = Vm::launch(&sandbox.config, &spec(&sandbox))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::Spawn { .. }), "{err}");
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn second_launch_of_same_vm_is_refused_and_first_is_untouched() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let err = Vm::launch(&sandbox.config, &spec(&sandbox))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::JailExists { .. }), "{err}");
    assert!(vm.paths().api_socket().exists());
    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn drop_without_shutdown_kills_reaps_and_removes_jail_and_cgroup() {
    let sandbox = Sandbox::new(Mode::IgnoreTerm);
    let vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let proc_dir = std::path::PathBuf::from(format!("/proc/{}", vm.pid().unwrap()));
    let paths = vm.paths().clone();
    assert!(paths.cgroup_dir.is_dir());
    drop(vm);
    // A zombie still has its /proc entry, so this also requires the process to be reaped.
    wait_until(
        "the dropped VM is reaped and its jail and cgroup removed",
        Duration::from_secs(5),
        || !proc_dir.exists() && !paths.jail_dir.exists() && !paths.cgroup_dir.exists(),
    )
    .await;
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn stalled_ctrl_alt_del_does_not_block_shutdown() {
    let sandbox = Sandbox::new(Mode::StallActions);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), vm.shutdown())
        .await
        .expect("shutdown blocked on an unanswered SendCtrlAltDel")
        .unwrap();
    assert_eq!(exit_state(&vm).await, killed_by(15));
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn shutdown_of_a_cleaned_handle_leaves_its_replacement_intact() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut first = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    first.shutdown().await.unwrap();
    let mut second = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();

    first.shutdown().await.unwrap();
    assert!(second.paths().api_socket().exists());
    assert!(second.paths().cgroup_dir.is_dir());
    let _: Option<serde_json::Value> = second.client().get("/").await.unwrap();
    second.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn concurrent_launches_of_same_vm_have_one_winner() {
    let sandbox = Sandbox::new(Mode::Serve);
    let spec = spec(&sandbox);
    let (a, b) = tokio::join!(
        Vm::launch(&sandbox.config, &spec),
        Vm::launch(&sandbox.config, &spec)
    );
    let (wins, losses): (Vec<_>, Vec<_>) = [a, b].into_iter().partition(Result::is_ok);
    assert_eq!((wins.len(), losses.len()), (1, 1));
    let mut winner = wins.into_iter().next().unwrap().unwrap();
    let err = losses.into_iter().next().unwrap().err().unwrap();
    assert!(matches!(err, Error::JailExists { .. }), "{err}");
    assert!(winner.paths().root.join("vmlinux.bin").exists());
    let _: Option<serde_json::Value> = winner.client().get("/").await.unwrap();
    winner.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn cancelled_launch_leaves_no_jail_and_retry_works() {
    let mut sandbox = Sandbox::new(Mode::Serve);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    sandbox.config.preparation_barrier = Some(std::sync::Arc::clone(&barrier));
    let test_spec = spec(&sandbox);
    let jail = sandbox.jail_dir("lab1-web");

    let barrier_wait = tokio::task::spawn_blocking({
        let b = std::sync::Arc::clone(&barrier);
        move || b.wait()
    });

    let cancelled = tokio::select! {
        biased;
        // Wait until `prepare_chroot` claims the jail and hits the barrier.
        () = async { barrier_wait.await.unwrap(); } => true,
        _ = Vm::launch(&sandbox.config, &test_spec) => false,
    };
    assert!(cancelled, "launch completed before cancellation");
    assert!(jail.exists());

    // Release the task so it can observe the cancellation (drop the unread result).
    tokio::task::spawn_blocking(move || barrier.wait())
        .await
        .unwrap();

    wait_until(
        "the cancelled launch is rolled back",
        Duration::from_secs(5),
        || !jail.exists(),
    )
    .await;

    sandbox.config.preparation_barrier = None;
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    vm.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn exit_with_readiness_request_in_flight_is_reported_at_once() {
    let sandbox = Sandbox::new(Mode::StallReadyThenExit);
    let mut spec = spec(&sandbox);
    spec.timeouts.ready = Duration::from_secs(30);
    let err = tokio::time::timeout(Duration::from_secs(10), Vm::launch(&sandbox.config, &spec))
        .await
        .expect("exit not noticed while the readiness request was in flight")
        .err()
        .unwrap();
    assert!(
        matches!(&err, Error::ExitedDuringStart { state, .. } if *state == exited(5)),
        "{err}"
    );
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn zero_metrics_poll_is_refused_before_spawning() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut spec = spec(&sandbox);
    spec.timeouts.metrics_poll = Duration::ZERO;
    let err = Vm::launch(&sandbox.config, &spec).await.err().unwrap();
    assert!(matches!(err, Error::ZeroMetricsPoll { .. }), "{err}");
    sandbox.assert_no_jail_left();
}

#[tokio::test]
#[allow(clippy::panic)]
async fn launch_fails_during_start_and_includes_output() {
    let sandbox = Sandbox::new(Mode::FailDuringStart);
    let res = Vm::launch(&sandbox.config, &spec(&sandbox)).await;
    match res {
        Err(Error::ExitedDuringStart { output, .. }) => {
            assert!(output.contains("fake jailer failed during start"));
        }
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => panic!("expected ExitedDuringStart, got {e:?}"),
    }
}

#[tokio::test]
async fn inherited_stderr_does_not_block_shutdown_or_reaping() {
    let sandbox = Sandbox::new(Mode::LeakOutputThenExit);
    let mut test_spec = spec(&sandbox);
    test_spec.timeouts.kill = Duration::from_millis(100);
    let mut vm = Vm::launch(&sandbox.config, &test_spec).await.unwrap();
    let proc_dir = std::path::PathBuf::from(format!("/proc/{}", vm.pid().unwrap()));
    tokio::time::timeout(Duration::from_secs(3), vm.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(!proc_dir.exists());
    sandbox.assert_no_jail_left();
    sandbox.release_output_holder().await;
}

#[tokio::test]
async fn cancelled_shutdown_retries_and_waits_for_cleanup() {
    let (logs, _guard) = Captured::install();
    let mut sandbox = Sandbox::new(Mode::Serve);
    let barrier = std::sync::Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    sandbox.config.cleanup_barrier = Some(barrier.clone());
    let mut test_spec = spec(&sandbox);
    test_spec.timeouts.kill = Duration::from_millis(100);
    let mut vm = Vm::launch(&sandbox.config, &test_spec).await.unwrap();
    let cancelled = tokio::select! {
        biased;
        () = barrier.0.notified() => true,
        _ = vm.shutdown() => false,
    };
    assert!(cancelled);
    assert!(vm.paths().jail_dir.exists());
    let cancelled = tokio::select! {
        biased;
        () = wait_until("forwarding handles are consumed", Duration::from_secs(5), || logs.contains("post-exit tasks joined")) => true,
        _ = vm.shutdown() => false,
    };
    assert!(cancelled);
    let error = vm.shutdown().await.unwrap_err();
    assert!(matches!(error, Error::CleanupTimeout { .. }));
    assert!(vm.paths().jail_dir.exists());
    barrier.1.notify_one();
    vm.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_retry_preserves_cleanup_failure() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let leftover = vm.paths().cgroup_dir.join("busy");
    std::fs::write(&leftover, "busy").unwrap();
    let error = vm.shutdown().await.unwrap_err();
    assert!(matches!(error, Error::Io { .. }));
    let error = vm.shutdown().await.unwrap_err();
    assert!(matches!(error, Error::CleanupFailed { .. }));
    std::fs::remove_file(leftover).unwrap();
    std::fs::remove_dir(&vm.paths().cgroup_dir).unwrap();
    std::fs::remove_dir_all(&vm.paths().jail_dir).unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn launch_passes_vm_resources_to_jailer_limits() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut spec = spec(&sandbox);
    spec.vcpu_count = 8;
    spec.mem_size_mib = 512;
    let mut vm = Vm::launch(&sandbox.config, &spec).await.unwrap();
    let args = std::fs::read_to_string(vm.paths().root.join("cgroup-args")).unwrap();
    assert_eq!(args, "pids.max=40\nmemory.max=671088640");
    vm.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn readiness_marker_retained_under_spam() {
    let sandbox = Sandbox::new(Mode::SpamOutput);
    let vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    // Firecracker output might take a moment to be forwarded.
    let res = vm
        .wait_for_line(
            "dylos: ready",
            "dylos: network setup failed",
            std::time::Duration::from_secs(3),
        )
        .await;
    assert!(
        res.is_ok(),
        "Readiness should be detected even if 30 lines follow it immediately"
    );
}

#[tokio::test]
#[allow(clippy::panic)]
async fn readiness_requires_exact_match() {
    let sandbox = Sandbox::new(Mode::FalseReadiness);
    let vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let res = vm
        .wait_for_line(
            "dylos: ready",
            "dylos: network setup failed",
            std::time::Duration::from_secs(3),
        )
        .await;
    match res {
        Err(dylos_runtime::Error::ReadinessFailed { line, .. }) => {
            assert_eq!(line, "dylos: network setup failed");
        }
        other => panic!("expected ReadinessFailed, got {other:?}"),
    }
}

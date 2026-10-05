mod support;

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use dylos_fc::config::{ActionType, InstanceActionInfo};
use dylos_runtime::jailer::ChrootFile;
use dylos_runtime::{Error, ProcessState, Timeouts, Vm, VmSpec};
use support::{Captured, Mode, Sandbox};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

#[allow(clippy::unwrap_used)]
fn spec(sandbox: &Sandbox) -> VmSpec {
    let kernel = sandbox.dir.path().join("vmlinux.bin");
    std::fs::write(&kernel, b"kernel").unwrap();
    VmSpec {
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

    assert_eq!(exit_state(&vm).await, exited(7));
    vm.shutdown().await.unwrap();
    assert!(logs.contains("Firecracker exited unexpectedly"));
    assert!(logs.contains("process_startup_time_us"));
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
async fn drop_without_shutdown_kills_and_removes_the_jail() {
    let sandbox = Sandbox::new(Mode::IgnoreTerm);
    let vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let proc_stat = format!("/proc/{}/stat", vm.pid().unwrap());
    drop(vm);
    sandbox.assert_no_jail_left();
    // SIGKILL lands once the runtime drops the aborted supervisor and its child.
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read_to_string(&proc_stat).is_ok_and(|s| !s.contains(") Z ")) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("process still running after drop");
}

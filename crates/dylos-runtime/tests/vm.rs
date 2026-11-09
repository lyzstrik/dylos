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
async fn launcher_has_its_own_process_group() {
    use nix::unistd::{Pid, getpgid, getpgrp};

    let sandbox = Sandbox::new(Mode::Serve);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let pid = Pid::from_raw(i32::try_from(vm.pid().unwrap()).unwrap());
    assert_eq!(getpgid(Some(pid)).unwrap(), pid);
    assert_ne!(getpgid(Some(pid)).unwrap(), getpgrp());
    vm.shutdown().await.unwrap();
    assert_eq!(exit_state(&vm).await, exited(0));
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
    assert!(
        matches!(&err, Error::ReadyTimeout { last_probe_error: Some(error), .. }
        if error.contains("Connect") && error.contains("NotFound")),
        "{err:?}"
    );
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
async fn launch_rejects_dynamic_firecracker_before_preparing_chroot() {
    let mut sandbox = Sandbox::new(Mode::Serve);
    let dynamic_fixture = support::elf::ElfBuilder::dynamic_binary()
        .write_temp()
        .unwrap();
    let dynamic = dynamic_fixture.path().to_path_buf();
    sandbox.config.firecracker = dynamic.clone();
    let err = Vm::launch(&sandbox.config, &spec(&sandbox))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, Error::DynamicFirecracker { path } if path == &dynamic),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("dynamically linked"), "{msg}");
    assert!(msg.contains("DYLOS_FC_BIN_DIR"), "{msg}");
    assert!(msg.contains("docs/host.md"), "{msg}");
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
async fn launch_rejects_zero_vcpu_count_before_spawning() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut spec = spec(&sandbox);
    spec.vcpu_count = 0;
    let err = Vm::launch(&sandbox.config, &spec).await.err().unwrap();
    assert!(
        matches!(
            err,
            Error::InvalidResources {
                vcpu_count: 0,
                mem_size_mib: 128,
                ..
            }
        ),
        "{err}"
    );
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn launch_rejects_vcpu_count_exceeding_32_before_spawning() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut spec = spec(&sandbox);
    spec.vcpu_count = 33;
    let err = Vm::launch(&sandbox.config, &spec).await.err().unwrap();
    assert!(
        matches!(
            err,
            Error::InvalidResources {
                vcpu_count: 33,
                mem_size_mib: 128,
                ..
            }
        ),
        "{err}"
    );
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn launch_rejects_odd_vcpu_count_above_1_before_spawning() {
    let sandbox = Sandbox::new(Mode::Serve);
    for odd_vcpus in [3, 5, 7, 31] {
        let mut spec = spec(&sandbox);
        spec.vcpu_count = odd_vcpus;
        let err = Vm::launch(&sandbox.config, &spec).await.err().unwrap();
        assert!(
            matches!(
                err,
                Error::InvalidResources {
                    vcpu_count,
                    mem_size_mib: 128,
                    ..
                } if vcpu_count == u32::from(odd_vcpus)
            ),
            "{err}"
        );
        sandbox.assert_no_jail_left();
    }
}

#[tokio::test]
async fn launch_rejects_zero_memory_size_before_spawning() {
    let sandbox = Sandbox::new(Mode::Serve);
    let mut spec = spec(&sandbox);
    spec.mem_size_mib = 0;
    let err = Vm::launch(&sandbox.config, &spec).await.err().unwrap();
    assert!(
        matches!(
            err,
            Error::InvalidResources {
                vcpu_count: 1,
                mem_size_mib: 0,
                ..
            }
        ),
        "{err}"
    );
    sandbox.assert_no_jail_left();
}

#[test]
fn validate_resources_accepts_valid_and_rejects_invalid() {
    use dylos_runtime::validate_resources;

    // Valid: 1 vCPU, even vCPUs up to 32, non-zero memory
    assert!(validate_resources(1, 128));
    assert!(validate_resources(2, 128));
    assert!(validate_resources(4, 256));
    assert!(validate_resources(32, 1024));

    // Invalid: vcpu == 0, vcpu > 32, odd vcpu > 1, mem == 0
    assert!(!validate_resources(0, 128));
    assert!(!validate_resources(33, 128));
    assert!(!validate_resources(34, 128));
    assert!(!validate_resources(3, 128));
    assert!(!validate_resources(5, 128));
    assert!(!validate_resources(1, 0));
    assert!(!validate_resources(0, 0));
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
    test_spec.timeouts.kill = Duration::from_millis(500);
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

async fn readiness_case(mode: Mode, forwarded: Option<&str>) -> dylos_runtime::Result<Duration> {
    let sandbox = Sandbox::new(mode);
    support::readiness_case(&sandbox, &spec(&sandbox), mode, forwarded).await
}

#[tokio::test]
async fn readiness_marker_emitted_before_waiting() {
    assert!(
        readiness_case(Mode::EarlyReadiness, Some("early marker forwarded"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn readiness_marker_retained_under_spam() {
    assert!(
        readiness_case(Mode::SpamOutput, Some("burst forwarded"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn readiness_requires_exact_match_and_retains_first_failure() {
    let result = readiness_case(Mode::FalseReadiness, None).await;
    assert!(
        matches!(result, Err(Error::ReadinessFailed { line, .. }) if line == "dylos: network setup failed")
    );
}

#[tokio::test]
async fn readiness_timeout_cleans_up() {
    assert!(matches!(
        readiness_case(Mode::Serve, None).await,
        Err(Error::ReadinessTimeout { .. })
    ));
}

#[tokio::test]
async fn exec_runs_commands_and_returns_output_and_exit_code() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let out = vm
        .exec("echo hello_world", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.trim(), "hello_world");
    assert!(out.success());

    let out = vm
        .exec("sh -c 'exit 42'", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 42);
    assert!(!out.success());

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_handles_no_trailing_newline() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let out = vm
        .exec("printf 'hello_without_newline'", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "hello_without_newline");
    assert!(out.success());

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_times_out_when_command_blocks() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let res = vm.exec("sleep 10", Duration::from_millis(100)).await;
    assert!(
        matches!(res, Err(Error::ExecTimeout { timeout, .. }) if timeout == Duration::from_millis(100))
    );

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_fails_when_output_exceeds_budget() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    // 1 MiB budget exceeded
    let res = vm
        .exec(
            "head -c 1100000 /dev/zero | tr '\\0' 'a'",
            Duration::from_secs(5),
        )
        .await;
    assert!(
        matches!(res, Err(Error::ExecFailed { ref message, .. }) if message.contains("exceeded budget")),
        "Expected budget overflow error, got: {res:?}"
    );

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_fails_when_blank_lines_exceed_limit() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    // Blank lines charge the line limit and budget, preventing unbounded accumulator growth
    let res = vm
        .exec("yes '' | head -n 1200", Duration::from_secs(5))
        .await;
    assert!(
        matches!(res, Err(Error::ExecFailed { ref message, .. }) if message.contains("exceeded limit of 1000 lines")),
        "Expected limit error on blank line flood, got: {res:?}"
    );

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_fails_when_many_short_lines_exceed_limit() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    // Short lines charge both per-line separators and the line limit
    let res = vm
        .exec("yes a | head -n 1200", Duration::from_secs(5))
        .await;
    assert!(
        matches!(res, Err(Error::ExecFailed { ref message, .. }) if message.contains("exceeded limit of 1000 lines")),
        "Expected limit error on short line flood, got: {res:?}"
    );

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_rejects_forged_markers() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    // A command that prints a dummy end marker does not cause premature exit or fake status
    let out = vm
        .exec(
            "echo '__DYLOS_END_0000000000000000__ 0'; echo real_output",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.stdout.contains("__DYLOS_END_0000000000000000__ 0"));
    assert!(out.stdout.contains("real_output"));

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_concurrent_calls_serialize_correctly() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let (res1, res2) = tokio::join!(
        vm.exec("echo call_one", Duration::from_secs(5)),
        vm.exec("echo call_two", Duration::from_secs(5)),
    );
    let out1 = res1.unwrap();
    let out2 = res2.unwrap();
    assert_eq!(out1.exit_code, 0);
    assert_eq!(out1.stdout.trim(), "call_one");
    assert_eq!(out2.exit_code, 0);
    assert_eq!(out2.stdout.trim(), "call_two");

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn exec_deadline_covers_mutex_acquisition() {
    let sandbox = Sandbox::new(Mode::Echo);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let gate_marker = sandbox.dir.path().join("exec-gate");
    let lock_marker = sandbox.dir.path().join("exec-lock-acquired");
    let release_marker = sandbox.dir.path().join("exec-release");

    // Gate completion of the first command until released by the test.
    std::fs::write(&gate_marker, b"1").unwrap();
    let slow = vm.exec("echo slow", Duration::from_secs(10));
    let blocked = async {
        // Wait until slow has acquired the stdin lock and entered the responder gate.
        support::wait_until("slow exec acquired lock", Duration::from_secs(5), || {
            lock_marker.exists()
        })
        .await;
        // Attempt a second exec with a short timeout (50ms); it must time out while waiting for the lock.
        let contender_res = vm.exec("echo blocked", Duration::from_millis(50)).await;
        // Release the first command now that the contender has timed out.
        std::fs::write(&release_marker, b"1").unwrap();
        contender_res
    };
    let (res_slow, res_blocked) = tokio::join!(slow, blocked);
    let slow_output = res_slow.unwrap();
    assert_eq!(slow_output.exit_code, 0);
    assert_eq!(slow_output.stdout.trim(), "slow");
    assert!(
        matches!(res_blocked, Err(Error::ExecTimeout { timeout, .. }) if timeout == Duration::from_millis(50)),
        "Expected timeout while waiting for lock, got: {res_blocked:?}"
    );

    vm.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_and_newline_free_output_is_bounded_and_recovers() {
    use std::io::Write;

    let (logs, _guard) = Captured::install();
    let sandbox = Sandbox::new(Mode::OversizedOutput);
    let mut vm = Vm::launch(&sandbox.config, &spec(&sandbox)).await.unwrap();
    let flush = InstanceActionInfo::new(ActionType::FlushMetrics);
    let _: Option<serde_json::Value> = vm.client().put("/actions", &flush).await.unwrap();
    wait_until(
        "both newline-free streams are truncated",
        Duration::from_secs(5),
        || {
            logs.0
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains("dylos::firecracker ") && line.contains("[truncated]"))
                .count()
                == 128
        },
    )
    .await;

    let _: Option<serde_json::Value> = vm.client().put("/actions", &flush).await.unwrap();
    wait_until(
        "console resumes after the oversized line",
        Duration::from_secs(5),
        || logs.contains("console-recovered"),
    )
    .await;

    wait_until(
        "both metrics flushes are consumed",
        Duration::from_secs(5),
        || {
            logs.0
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains("process_startup_time_us"))
                .count()
                == 2
        },
    )
    .await;

    let mut metrics = std::fs::OpenOptions::new()
        .append(true)
        .open(vm.paths().host_path(dylos_runtime::jailer::METRICS_FILE))
        .unwrap();
    metrics.write_all(&vec![b'm'; 256 * 1024]).unwrap();
    metrics.flush().unwrap();
    wait_until(
        "newline-free metrics are truncated",
        Duration::from_secs(5),
        || {
            logs.0.lock().unwrap().iter().any(|line| {
                line.contains("dylos::firecracker::metrics") && line.contains("[truncated]")
            })
        },
    )
    .await;
    metrics.write_all(b"\nmetrics-recovered\n").unwrap();
    metrics.flush().unwrap();
    wait_until(
        "metrics resume after the oversized line",
        Duration::from_secs(5),
        || logs.contains("metrics-recovered"),
    )
    .await;
    {
        let lines = logs.0.lock().unwrap();
        for line in lines.iter().filter(|line| line.contains("[truncated]")) {
            let limit = if line.contains("dylos::firecracker::metrics") {
                66_000
            } else {
                4300
            };
            assert!(line.len() < limit, "oversized event: {} bytes", line.len());
        }
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("[truncated]"))
                .count(),
            132
        );
    }
    vm.shutdown().await.unwrap();
    sandbox.assert_no_jail_left();
}

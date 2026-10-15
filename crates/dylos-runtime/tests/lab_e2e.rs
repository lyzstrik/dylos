#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig, ProcessState, pause_all, resume_all};
use futures_util::future::join_all;

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn abc_lab_ready_then_control_socket_teardown_leaves_nothing() {
    run_lab(false).await;
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn abc_lab_terminal_sigint_is_graceful() {
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "terminal_sigint_helper",
            "--nocapture",
        ])
        .env("DYLOS_TEST_TERMINAL_SIGINT", "1")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(120), child.wait())
        .await
        .expect("isolated supervisor timed out")
        .unwrap();
    assert!(status.success(), "isolated supervisor failed: {status}");
}

#[tokio::test]
#[ignore = "subprocess helper for terminal SIGINT e2e"]
async fn terminal_sigint_helper() {
    if std::env::var_os("DYLOS_TEST_TERMINAL_SIGINT").is_some() {
        run_lab(true).await;
    }
}

async fn run_lab(terminal_sigint: bool) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::{Pid, getpgid, getpgrp};

    // Install before sending to the isolated supervisor group, as the CLI does.
    let mut interrupt =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let Some(config) = lab_config(&dir) else {
        return;
    };
    let id = if terminal_sigint {
        "abc-sigint-e2e"
    } else {
        "abc-e2e"
    };
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    eprintln!(
        "lab_e2e: config={config:?}; netns={:?}; mountns={:?}",
        std::fs::read_link("/proc/self/ns/net"),
        std::fs::read_link("/proc/self/ns/mnt")
    );
    let result = Lab::up_with(&config, &spec, id).await;
    if let Err(error) = &result {
        eprintln!("lab bring-up failed: {error}\n{error:#?}");
    }
    let mut lab = result.expect("lab bring-up failed; see diagnostics above");
    assert_eq!(lab.vms().len(), 3);
    join_all(lab.vms().iter().map(|vm| async move {
        let mut state = vm.state();
        assert!(
            tokio::time::timeout(
                Duration::from_secs(3),
                state.wait_for(|state| *state != ProcessState::Running),
            )
            .await
            .is_err(),
            "VM {} exited within three seconds of readiness: {:?}",
            vm.paths().id,
            *state.borrow(),
        );
    }))
    .await;
    measure_pause_cycles(&mut lab).await;
    let paths: Vec<_> = lab
        .vms()
        .iter()
        .map(|vm| (vm.paths().clone(), vm.pid().unwrap()))
        .collect();
    let states: Vec<_> = lab.vms().iter().map(dylos_runtime::Vm::state).collect();
    for (_, pid) in &paths {
        let pid = Pid::from_raw(i32::try_from(*pid).unwrap());
        assert_eq!(getpgid(Some(pid)).unwrap(), pid);
        assert_ne!(getpgid(Some(pid)).unwrap(), getpgrp());
    }
    let socket = lab.control_socket();
    let root = lab.root().to_owned();
    assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
    let server = tokio::spawn(lab.supervise());
    if terminal_sigint {
        killpg(getpgrp(), Signal::SIGINT).unwrap();
        tokio::time::timeout(Duration::from_secs(5), interrupt.recv())
            .await
            .expect("supervisor did not receive SIGINT")
            .expect("signal stream closed");
        for state in &states {
            assert_eq!(*state.borrow(), ProcessState::Running);
        }
    }
    Lab::request_teardown(&socket, Duration::from_secs(60))
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    for state in states {
        assert!(
            matches!(&*state.borrow(), ProcessState::Exited(exit) if exit.success()),
            "VM did not exit gracefully: {:?}",
            *state.borrow()
        );
    }
    for (paths, pid) in paths {
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(!paths.jail_dir.exists());
        assert!(!paths.cgroup_dir.exists());
    }
    assert!(!root.exists());
    assert!(!socket.exists());
    let netns = config.netns_dir.join(format!("dylos-{id}"));
    assert!(!netns.exists());
    assert!(
        !std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .contains(netns.to_str().unwrap())
    );
    Lab::teardown_by_name(&config, id).await.unwrap();
}

fn lab_config(dir: &tempfile::TempDir) -> Option<LabConfig> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED lab_e2e: DYLOS_FC_BIN_DIR is not set");
        return None;
    };
    if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
        eprintln!("SKIPPED lab_e2e: must start as root");
        return None;
    }
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");
    for path in [
        Path::new("/dev/kvm"),
        &bin.join("jailer"),
        &bin.join("firecracker"),
        &kernel,
        &image,
    ] {
        if !path.exists() {
            eprintln!("SKIPPED lab_e2e: {} is missing", path.display());
            return None;
        }
    }
    let mut jailer = JailerConfig::new(dir.path().join("jails"), 65534, 65534);
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
        boot_timeout: Duration::from_secs(30),
        ..LabConfig::default()
    };
    Some(config)
}

async fn measure_pause_cycles(lab: &mut Lab) {
    let mut total = Duration::ZERO;
    let mut maximum = Duration::ZERO;
    for _ in 0..50 {
        let skew = pause_all(lab).await.expect("pause cycle failed");
        total += skew;
        maximum = maximum.max(skew);
        resume_all(lab).await.expect("resume cycle failed");
    }
    assert!(
        maximum < Duration::from_millis(100),
        "pause skew: {maximum:?}"
    );
    eprintln!(
        "abc pause/resume: cycles=50 mean_skew_us={} maximum_skew_us={}",
        (total / 50).as_micros(),
        maximum.as_micros()
    );
}

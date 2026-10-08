#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Command;
use std::time::Duration;

use dylos_core::{LabSpec, guest_net};
use dylos_runtime::{Lab, LabConfig, LabError, Timeouts};
use support::{Mode, Sandbox};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn fake_jailer_entry() {
    support::run_fake_jailer_if_requested();
}

fn sandboxed(test: &str) -> bool {
    if std::env::var_os("DYLOS_LAB_SANDBOX").is_some() {
        return true;
    }
    if !Command::new("unshare")
        .args(["-Urmn", "true"])
        .status()
        .is_ok_and(|s| s.success())
    {
        eprintln!("SKIPPED {test}: unprivileged user namespaces unavailable");
        return false;
    }
    let status = Command::new("unshare")
        .arg("-Urmn")
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env("DYLOS_LAB_SANDBOX", "1")
        .status()
        .unwrap();
    assert!(status.success());
    false
}

fn setup(mode: Mode) -> (Sandbox, LabConfig, LabSpec) {
    let sandbox = Sandbox::in_target(mode);
    let kernel = sandbox.dir.path().join("vmlinux.bin");
    fs::write(&kernel, b"kernel").unwrap();
    let image = sandbox.dir.path().join("rootfs.ext4");
    fs::write(&image, b"immutable rootfs").unwrap();
    let config = LabConfig {
        store: dylos_store::Store {
            base: sandbox.dir.path().join("labs"),
            image,
        },
        kernel,
        netns_dir: sandbox.dir.path().join("netns"),
        jailer: sandbox.config.clone(),
        timeouts: Timeouts {
            ready: Duration::from_secs(2),
            graceful: Duration::from_millis(100),
            term: Duration::from_millis(100),
            kill: Duration::from_secs(2),
            metrics_poll: Duration::from_millis(50),
        },
        boot_timeout: Duration::from_millis(200),
    };
    let spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    (sandbox, config, spec)
}

fn supported(config: &LabConfig, spec: &LabSpec) -> Result<bool, dylos_store::Error> {
    match config.store.create("probe", spec) {
        Ok(mut lab) => {
            lab.remove().unwrap();
            Ok(true)
        }
        Err(dylos_store::Error::Reflink { source, .. })
            if source.raw_os_error().is_some_and(|code| {
                matches!(
                    nix::errno::Errno::from_raw(code),
                    nix::errno::Errno::EOPNOTSUPP
                        | nix::errno::Errno::ENOTTY
                        | nix::errno::Errno::EXDEV
                        | nix::errno::Errno::EINVAL
                )
            }) =>
        {
            eprintln!("SKIPPED lab test: reflinks unavailable: {source}");
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn assert_removed(sandbox: &Sandbox, config: &LabConfig) {
    sandbox.assert_no_jail_left();
    assert!(!config.store.base.join("lab").exists());
    assert!(!config.netns_dir.join("dylos-lab").exists());
}

#[tokio::test]
async fn bring_up_configures_all_vms_and_socket_teardown_removes_everything() {
    if !sandboxed("bring_up_configures_all_vms_and_socket_teardown_removes_everything") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let (logs, _guard) = support::Captured::install();
    let result = Lab::up_with(&config, &spec, "lab").await;
    assert!(result.is_ok(), "{:?}", logs.0.lock().unwrap());
    let lab = result.unwrap();
    let socket = lab.control_socket();
    let metadata = fs::metadata(&socket).unwrap();
    assert_eq!(metadata.uid(), 0);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(lab.vms().len(), 3);
    let mut pids = Vec::new();
    for (vm, node) in lab.vms().iter().zip(&spec.nodes) {
        pids.push(vm.pid().unwrap());
        let root = &vm.paths().root;
        assert_eq!(
            fs::read_to_string(root.join("netns-arg")).unwrap(),
            config.netns_dir.join("dylos-lab").to_str().unwrap()
        );
        let requests = fs::read_to_string(root.join("requests")).unwrap();
        assert!(requests.contains(&guest_net::boot_args(&spec, node).unwrap()));
        assert!(requests.contains("\"is_root_device\":true"));
        assert!(requests.contains("/rootfs.ext4"));
        for interface in &node.interfaces {
            assert!(
                requests.contains(
                    &guest_net::mac_for_interface(&node.name, &interface.name).to_string()
                )
            );
            assert!(requests.contains(&format!("tap-{}-{}", node.name, interface.name)));
        }
    }
    assert!(Lab::teardown_by_name(&config, "lab").await.is_err());
    assert!(Lab::up_with(&config, &spec, "lab").await.is_err());
    let server = tokio::spawn(lab.supervise());
    Lab::request_teardown(&socket, Duration::from_secs(5))
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    assert_removed(&sandbox, &config);
    for pid in pids {
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    Lab::teardown_by_name(&config, "lab").await.unwrap();
    Lab::teardown_by_name(&config, "lab").await.unwrap();
}

#[tokio::test]
async fn partial_launch_api_failure_readiness_failure_and_timeout_roll_back() {
    if !sandboxed("partial_launch_api_failure_readiness_failure_and_timeout_roll_back") {
        return;
    }
    for mode in [
        Mode::LabPartialLaunch,
        Mode::LabConfigureFailure,
        Mode::LabNetworkFailure,
        Mode::Serve,
    ] {
        let (sandbox, config, spec) = setup(mode);
        if !supported(&config, &spec).unwrap() {
            return;
        }
        let error = Lab::up_with(&config, &spec, "lab").await.err().unwrap();
        match mode {
            Mode::LabPartialLaunch => assert!(matches!(
                error,
                LabError::Vm(dylos_runtime::Error::ExitedDuringStart { .. })
            )),
            Mode::LabConfigureFailure => {
                assert!(matches!(&error, LabError::Api { .. }), "{error:?}");
            }
            Mode::LabNetworkFailure => assert!(matches!(
                error,
                LabError::Vm(dylos_runtime::Error::ReadinessFailed { .. })
            )),
            Mode::Serve => assert!(matches!(
                error,
                LabError::Vm(dylos_runtime::Error::ReadinessTimeout { .. })
            )),
            _ => unreachable!(),
        }
        assert_removed(&sandbox, &config);
    }
}

#[tokio::test]
async fn invalid_inputs_are_rejected_before_creating_host_resources() {
    let (sandbox, config, mut spec) = setup(Mode::LabReady);
    for id in ["", "../escape", "a/b", "lab_1", &"a".repeat(33)] {
        assert!(Lab::up_with(&config, &spec, id).await.is_err());
        assert!(Lab::teardown_by_name(&config, id).await.is_err());
    }
    spec.nodes[0].name = "../escape".into();
    assert!(Lab::up_with(&config, &spec, "lab").await.is_err());
    assert!(!config.store.base.exists());
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn recovery_refuses_symlinks_mismatched_state_and_live_locks() {
    let (sandbox, config, spec) = setup(Mode::LabReady);
    let root = config.store.base.join("lab");
    fs::create_dir_all(&config.store.base).unwrap();
    let outside = sandbox.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"keep").unwrap();
    symlink(&outside, &root).unwrap();
    assert!(Lab::teardown_by_name(&config, "lab").await.is_err());
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(outside.join("sentinel"), root.join("state.json")).unwrap();
    assert!(Lab::teardown_by_name(&config, "lab").await.is_err());
    fs::remove_file(root.join("state.json")).unwrap();
    let state = dylos_store::State {
        lab_id: "other".into(),
        spec,
        created_at: std::time::SystemTime::now(),
    };
    fs::write(root.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(Lab::teardown_by_name(&config, "lab").await.is_err());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"keep");
    fs::remove_file(root.join("state.json")).unwrap();
    let lock = nix::fcntl::Flock::lock(
        fs::File::open(&root).unwrap(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .unwrap();
    assert!(Lab::teardown_by_name(&config, "lab").await.is_err());
    assert!(root.exists());
    drop(lock);
    Lab::teardown_by_name(&config, "lab").await.unwrap();
    assert!(!root.exists());
}

#[tokio::test]
async fn dropped_lab_and_double_teardown_release_owned_resources() {
    if !sandboxed("dropped_lab_and_double_teardown_release_owned_resources") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let mut lab = Lab::up_with(&config, &spec, "lab").await.unwrap();
    lab.teardown().await.unwrap();
    lab.teardown().await.unwrap();
    assert_removed(&sandbox, &config);
    let lab = Lab::up_with(&config, &spec, "lab").await.unwrap();
    drop(lab);
    support::wait_until("lab removed on drop", Duration::from_secs(5), || {
        !config.store.base.join("lab").exists()
    })
    .await;
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn invalid_control_request_does_not_stop_the_supervisor() {
    if !sandboxed("invalid_control_request_does_not_stop_the_supervisor") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let lab = Lab::up_with(&config, &spec, "lab").await.unwrap();
    let socket = lab.control_socket();
    let server = tokio::spawn(lab.supervise());
    let mut attacker = tokio::net::UnixStream::connect(&socket).await.unwrap();
    attacker.write_all(b"badinput\n").await.unwrap();
    assert_eq!(attacker.read(&mut [0; 1]).await.unwrap(), 0);
    assert!(config.store.base.join("lab").exists());
    Lab::request_teardown(&socket, Duration::from_secs(5))
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    assert_removed(&sandbox, &config);
}

fn stale_state(config: &LabConfig, spec: &LabSpec) -> std::path::PathBuf {
    let root = config.store.base.join("lab");
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let state = dylos_store::State {
        lab_id: "lab".into(),
        spec: spec.clone(),
        created_at: std::time::SystemTime::now(),
    };
    fs::write(root.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
    root
}

#[tokio::test]
async fn crash_recovery_cleans_owned_jails_and_cgroups_without_saved_pids() {
    let (sandbox, config, spec) = setup(Mode::LabReady);
    stale_state(&config, &spec);
    for node in &spec.nodes {
        let paths =
            dylos_runtime::jailer::JailPaths::new(&config.jailer, "lab3-lab", &node.name).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(paths.root.join("stale-file"), b"stale").unwrap();
        fs::create_dir_all(&paths.cgroup_dir).unwrap();
    }
    // This would collide with the old lab-node concatenation for lab `lab-A`, node `B`.
    let unrelated =
        dylos_runtime::jailer::JailPaths::new(&config.jailer, "lab5-lab-A", "B").unwrap();
    fs::create_dir_all(&unrelated.root).unwrap();
    fs::write(unrelated.root.join("sentinel"), b"keep").unwrap();
    Lab::teardown_by_name(&config, "lab").await.unwrap();
    Lab::teardown_by_name(&config, "lab").await.unwrap();
    assert!(!config.store.base.join("lab").exists());
    assert_eq!(fs::read(unrelated.root.join("sentinel")).unwrap(), b"keep");
    fs::remove_dir_all(&unrelated.jail_dir).unwrap();
    sandbox.assert_no_jail_left();
}

#[tokio::test]
async fn crash_recovery_kills_populated_cgroup_before_removing_jail() {
    let (sandbox, config, spec) = setup(Mode::LabReady);
    stale_state(&config, &spec);
    let paths = dylos_runtime::jailer::JailPaths::new(&config.jailer, "lab3-lab", "A").unwrap();
    fs::create_dir_all(&paths.root).unwrap();
    fs::create_dir_all(&paths.cgroup_dir).unwrap();
    fs::write(paths.cgroup_dir.join("cgroup.events"), b"populated 1\n").unwrap();
    fs::write(paths.cgroup_dir.join("cgroup.kill"), b"0").unwrap();
    let cgroup = paths.cgroup_dir.clone();
    let jail = paths.jail_dir.clone();
    let observer = tokio::spawn(async move {
        support::wait_until("cgroup kill requested", Duration::from_secs(5), || {
            fs::read(cgroup.join("cgroup.kill")).is_ok_and(|bytes| bytes == b"1")
        })
        .await;
        assert!(jail.exists(), "jail removed before cgroup stopped");
        fs::remove_file(cgroup.join("cgroup.kill")).unwrap();
        fs::remove_file(cgroup.join("cgroup.events")).unwrap();
    });
    Lab::teardown_by_name(&config, "lab").await.unwrap();
    observer.await.unwrap();
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn recovery_cgroup_symlink_cannot_write_outside_the_owned_tree() {
    let (sandbox, config, spec) = setup(Mode::LabReady);
    stale_state(&config, &spec);
    let paths = dylos_runtime::jailer::JailPaths::new(&config.jailer, "lab3-lab", "A").unwrap();
    fs::create_dir_all(paths.cgroup_dir.parent().unwrap()).unwrap();
    let outside = sandbox.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("cgroup.events"), b"populated 1\n").unwrap();
    fs::write(outside.join("cgroup.kill"), b"keep").unwrap();
    symlink(&outside, &paths.cgroup_dir).unwrap();
    assert!(Lab::teardown_by_name(&config, "lab").await.is_err());
    assert_eq!(fs::read(outside.join("cgroup.kill")).unwrap(), b"keep");
    assert!(config.store.base.join("lab").exists());
}

#[tokio::test]
async fn unexpected_vm_exit_tears_down_the_entire_lab() {
    if !sandboxed("unexpected_vm_exit_tears_down_the_entire_lab") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabCrashOnFlush);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let lab = Lab::up_with(&config, &spec, "lab").await.unwrap();
    let client = dylos_fc::FcClient::new(lab.vms()[0].paths().api_socket());
    let server = tokio::spawn(lab.supervise());
    client
        .put::<_, serde_json::Value>(
            "/actions",
            &dylos_fc::config::InstanceActionInfo::new(dylos_fc::config::ActionType::FlushMetrics),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_removed(&sandbox, &config);
}

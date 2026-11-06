#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Command;
use std::time::Duration;

use dylos_core::{LabSpec, guest_net};
use dylos_runtime::{Fleet, Lab, LabConfig, LabError, Timeouts};
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
        extra_boot_args: None,
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
    let error = Lab::teardown_by_name(&config, "lab").await.unwrap_err();
    assert!(matches!(
        error,
        LabError::Io { lab: ref id, operation: "lock lab directory", ref path, ref source }
            if id == "lab"
                && path == lab.root()
                && source.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(Lab::up_with(&config, &spec, "lab").await.is_err());
    let server = tokio::spawn(lab.supervise());
    Lab::request_teardown("lab", &socket, Duration::from_secs(5))
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
async fn invalid_vm_resources_are_rejected_before_creating_host_resources() {
    let (sandbox, config, mut spec) = setup(Mode::LabReady);
    for (vcpus, memory) in [(0, 128), (33, 128), (3, 128), (1, 0)] {
        spec.nodes[0].vcpus = vcpus;
        spec.nodes[0].memory = memory;
        let err = Lab::up_with(&config, &spec, "lab").await.err().unwrap();
        assert!(
            matches!(
                &err,
                LabError::Invalid { message, .. } if message.contains("invalid VM vCPU or memory configuration")
            ),
            "expected LabError::Invalid for vcpus={vcpus}, memory={memory}, got: {err}"
        );
        assert!(!config.store.base.exists());
        assert_removed(&sandbox, &config);
    }
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
    Lab::request_teardown("lab", &socket, Duration::from_secs(5))
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

#[tokio::test]
async fn restore_loads_frozen_then_resumes_and_supervises() {
    if !sandboxed("restore_loads_frozen_then_resumes_and_supervises") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let mut source = Lab::up_with(&config, &spec, "src").await.unwrap();
    let manifest = source.snapshot("saved").await.unwrap();
    let snapshot = config.store.snapshot_dir("src", "saved");
    let lab = Lab::restore_with(&config, &snapshot, "lab").await.unwrap();
    for vm in lab.vms() {
        let requests = fs::read_to_string(vm.paths().root.join("requests")).unwrap();
        assert!(requests.contains("PUT /snapshot/load "));
        assert!(requests.contains("Resumed"));
        assert!(!requests.contains("InstanceStart"));
        assert!(!requests.contains("PUT /machine-config"));
    }
    let socket = lab.control_socket();
    let server = tokio::spawn(lab.supervise());
    Lab::request_teardown("lab", &socket, Duration::from_secs(5))
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    source.teardown().await.unwrap();
    manifest
        .verify(
            dylos_store::SnapshotOpener::new(&snapshot)
                .unwrap()
                .verify_opener(),
        )
        .unwrap();
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn restore_load_and_resume_failures_roll_back_all_resources() {
    if !sandboxed("restore_load_and_resume_failures_roll_back_all_resources") {
        return;
    }
    for stage in ["load", "resume"] {
        let (sandbox, config, spec) = setup(Mode::LabReady);
        if !supported(&config, &spec).unwrap() {
            return;
        }
        let mut source = Lab::up_with(&config, &spec, "src").await.unwrap();
        source.snapshot("saved").await.unwrap();
        source.teardown().await.unwrap();
        fs::write(sandbox.dir.path().join("fail-restore"), stage).unwrap();
        assert!(
            Lab::restore_with(&config, &config.store.snapshot_dir("src", "saved"), "lab")
                .await
                .is_err()
        );
        assert_removed(&sandbox, &config);
        Lab::teardown_by_name(&config, "lab").await.unwrap();
        assert_removed(&sandbox, &config);
    }
}

#[test]
fn generate_clone_ids_validates_count_and_naming() {
    assert!(Lab::generate_clone_ids("test", 0).is_err());
    assert!(Lab::generate_clone_ids("test", 1025).is_err());
    assert!(Lab::generate_clone_ids("invalid/id", 1).is_err());
    assert!(Lab::generate_clone_ids("a-very-long-prefix-that-exceeds-32-chars", 1).is_err());
    // Prefix that fits for c1 but c100 exceeds 32 chars
    assert!(Lab::generate_clone_ids("twenty-seven-chars-long-prefix-", 100).is_err());
    let ids = Lab::generate_clone_ids("abc", 3).unwrap();
    assert_eq!(ids, vec!["abc-c1", "abc-c2", "abc-c3"]);
}

#[tokio::test]
async fn fork_with_rejects_empty_and_oversized_clone_lists() {
    let config = LabConfig::default();
    let snapshot = std::path::Path::new("missing");
    assert!(Lab::fork_with(&config, snapshot, &[]).await.is_err());
    let ids: Vec<String> = (0..=Lab::MAX_CLONES).map(|i| format!("c{i}")).collect();
    let Err(err) = Lab::fork_with(&config, snapshot, &ids).await else {
        panic!("oversized clone list accepted");
    };
    assert!(
        err.to_string().contains("clone identifiers required"),
        "{err}"
    );
}

#[tokio::test]
async fn fork_restores_clones_in_parallel_with_deterministic_ids_and_measures_latency_and_pss() {
    if !sandboxed(
        "fork_restores_clones_in_parallel_with_deterministic_ids_and_measures_latency_and_pss",
    ) {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let mut source = Lab::up_with(&config, &spec, "s").await.unwrap();
    source.snapshot("saved").await.unwrap();
    let snapshot = config.store.snapshot_dir("s", "saved");

    let clones = Lab::fork(&config, &snapshot, "s", 3).await.unwrap();
    assert_eq!(clones.len(), 3);
    let node0 = &spec.nodes[0].name;
    let first_mem = clones[0].root().join("vms").join(node0).join("vm.mem");
    let first_ino = fs::metadata(&first_mem).unwrap().ino();
    assert!(fs::metadata(&first_mem).unwrap().permissions().readonly());

    for (i, clone) in clones.iter().enumerate() {
        assert_eq!(clone.id(), format!("s-c{}", i + 1));
        assert!(clone.restore_latency().is_some());
        let clone_mem = clone.root().join("vms").join(node0).join("vm.mem");
        assert_eq!(fs::metadata(&clone_mem).unwrap().ino(), first_ino);
        assert!(fs::metadata(&clone_mem).unwrap().permissions().readonly());
        let pss = clone.measure_pss().await.unwrap();
        assert_eq!(pss.len(), 3);
        for (_node, pss_kib) in pss {
            assert!(pss_kib > 0);
        }
        assert!(clone.total_pss_kib().await.unwrap() > 0);
    }

    // Each clone can be supervised and torn down independently
    for clone in clones {
        let socket = clone.control_socket();
        let clone_id = clone.id().to_string();
        let server = tokio::spawn(clone.supervise());
        Lab::request_teardown(&clone_id, &socket, Duration::from_secs(5))
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        assert!(!config.store.base.join(&clone_id).exists());
        assert!(!config.netns_dir.join(format!("dylos-{clone_id}")).exists());
    }

    source.teardown().await.unwrap();
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn fork_partial_failure_rolls_back_all_clones() {
    if !sandboxed("fork_partial_failure_rolls_back_all_clones") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let mut source = Lab::up_with(&config, &spec, "src").await.unwrap();
    source.snapshot("saved").await.unwrap();
    let snapshot = config.store.snapshot_dir("src", "saved");

    // Block one clone from being restored by pre-creating an invalid directory with no write permissions
    let blocked_dir = config.store.base.join("fail-c2");
    fs::create_dir_all(&blocked_dir).unwrap();
    let mut perms = fs::metadata(&blocked_dir).unwrap().permissions();
    perms.set_mode(0o400);
    fs::set_permissions(&blocked_dir, perms).unwrap();

    let clone_ids = vec![
        "fail-c1".to_string(),
        "fail-c2".to_string(),
        "fail-c3".to_string(),
    ];
    let res = Lab::fork_with(&config, &snapshot, &clone_ids).await;
    assert!(res.is_err());

    // Restore permissions so cleanup works
    let mut perms = fs::metadata(&blocked_dir).unwrap().permissions();
    perms.set_mode(0o700);
    let _ = fs::set_permissions(&blocked_dir, perms);
    let _ = fs::remove_dir_all(&blocked_dir);

    // Assert fail-c1 and fail-c3 were rolled back
    assert!(!config.store.base.join("fail-c1").exists());
    assert!(!config.store.base.join("fail-c3").exists());
    assert!(!config.netns_dir.join("dylos-fail-c1").exists());
    assert!(!config.netns_dir.join("dylos-fail-c3").exists());

    source.teardown().await.unwrap();
    assert_removed(&sandbox, &config);
}

fn assert_lab_removed(sandbox: &Sandbox, config: &LabConfig, ids: &[&str]) {
    sandbox.assert_no_jail_left();
    for id in ids {
        assert!(!config.store.base.join(id).exists(), "{id} storage left");
        assert!(
            !config.netns_dir.join(format!("dylos-{id}")).exists(),
            "{id} netns left"
        );
    }
}

fn crash(lab: &Lab) -> dylos_fc::FcClient {
    dylos_fc::FcClient::new(lab.vms()[0].paths().api_socket())
}

async fn flush(client: &dylos_fc::FcClient) {
    client
        .put::<_, serde_json::Value>(
            "/actions",
            &dylos_fc::config::InstanceActionInfo::new(dylos_fc::config::ActionType::FlushMetrics),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn fleet_recovers_failed_and_panicked_labs_and_keeps_the_others() {
    if !sandboxed("fleet_recovers_failed_and_panicked_labs_and_keeps_the_others") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabCrashOnFlush);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let mut fleet = Fleet::new(&config, Duration::from_secs(5));
    let failing = Lab::up_with(&config, &spec, "f1").await.unwrap();
    let client = crash(&failing);
    fleet.spawn(failing);
    let panicking = Lab::up_with(&config, &spec, "f2").await.unwrap();
    let socket = panicking.control_socket();
    fleet.spawn_supervisor("f2".into(), socket, async move {
        let _owned = panicking;
        panic!("injected supervisor panic");
    });
    let healthy = Lab::up_with(&config, &spec, "f3").await.unwrap();
    let healthy_socket = healthy.control_socket();
    fleet.spawn(healthy);
    let run = tokio::spawn(fleet.run(std::future::pending()));
    flush(&client).await;
    support::wait_until("failed labs recovered", Duration::from_secs(10), || {
        ["f1", "f2"]
            .iter()
            .all(|id| !config.store.base.join(id).exists())
    })
    .await;
    assert!(config.store.base.join("f3").exists());
    // As `dylos lab down f3`: the run ends once the last lab is down.
    Lab::request_teardown("f3", &healthy_socket, Duration::from_secs(5))
        .await
        .unwrap();
    let failures = run.await.unwrap().unwrap_err().0;
    let failed: Vec<_> = failures.iter().map(|f| f.lab.as_str()).collect();
    assert_eq!(failed.len(), 2, "{failures:?}");
    assert!(
        failed.contains(&"f1") && failed.contains(&"f2"),
        "{failures:?}"
    );
    for failure in &failures {
        // The supervisor error only: recovery by name succeeded.
        assert_eq!(failure.errors.len(), 1, "{failure}");
        match failure.lab.as_str() {
            "f1" => assert!(
                failure.to_string().contains("VM exited unexpectedly"),
                "{failure}"
            ),
            _ => assert!(
                matches!(failure.errors[0], LabError::Task { .. }),
                "{failure}"
            ),
        }
    }
    assert_lab_removed(&sandbox, &config, &["f1", "f2", "f3"]);
}

#[tokio::test]
async fn fleet_shutdown_aborts_stalled_and_unreachable_supervisors_then_recovers() {
    if !sandboxed("fleet_shutdown_aborts_stalled_and_unreachable_supervisors_then_recovers") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let timeout = Duration::from_secs(30);
    let mut fleet = Fleet::new(&config, timeout);
    // Holds the lab and its bound socket but never answers: the request times out.
    let stalled = Lab::up_with(&config, &spec, "f1").await.unwrap();
    let socket = stalled.control_socket();
    fleet.spawn_supervisor("f1".into(), socket, async move {
        let _owned = stalled;
        std::future::pending().await
    });
    let unreachable = Lab::up_with(&config, &spec, "f2").await.unwrap();
    fs::remove_file(unreachable.control_socket()).unwrap();
    fleet.spawn(unreachable);
    let healthy = Lab::up_with(&config, &spec, "f3").await.unwrap();
    let (healthy_done, mut healthy_stopped) = tokio::sync::oneshot::channel();
    fleet.spawn_supervisor("f3".into(), healthy.control_socket(), async move {
        let result = healthy.supervise().await;
        healthy_done.send(()).unwrap();
        result
    });

    // A real 500 ms fleet deadline races host cleanup (VM cleanup alone allows 2 s).
    // Keep time frozen until the healthy response, then expire only the stalled request.
    tokio::time::pause();
    let shutdown = tokio::spawn(fleet.run(std::future::ready(())));
    let started = std::time::Instant::now();
    loop {
        match healthy_stopped.try_recv() {
            Ok(()) => break,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Err(error) => panic!("healthy supervisor dropped completion signal: {error}"),
        }
        assert!(started.elapsed() < timeout, "healthy teardown stalled");
        // Remaining runnable prevents Tokio from automatically advancing paused time.
        tokio::task::yield_now().await;
    }
    tokio::time::advance(timeout).await;
    tokio::time::resume();
    let failures = shutdown.await.unwrap().unwrap_err().0;
    assert_eq!(failures.len(), 2, "{failures:?}");
    for failure in &failures {
        let kind = match failure.lab.as_str() {
            "f1" => std::io::ErrorKind::TimedOut,
            "f2" => std::io::ErrorKind::NotFound,
            other => panic!("unexpected failure of {other}"),
        };
        assert_eq!(failure.errors.len(), 1, "{failure}");
        assert!(
            matches!(&failure.errors[0], LabError::Io { source, .. } if source.kind() == kind),
            "{failure}"
        );
    }
    assert_lab_removed(&sandbox, &config, &["f1", "f2", "f3"]);
}

#[tokio::test]
async fn fleet_reports_the_recovery_error_with_the_operation_error() {
    if !sandboxed("fleet_reports_the_recovery_error_with_the_operation_error") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    let mut fleet = Fleet::new(&config, Duration::from_millis(300));
    // The lab outlives its supervisor, so its lock is never released for recovery.
    let mut kept = Lab::up_with(&config, &spec, "f1").await.unwrap();
    fleet.spawn_supervisor("f1".into(), kept.control_socket(), std::future::pending());

    let failures = fleet.run(std::future::ready(())).await.unwrap_err().0;
    assert_eq!(failures.len(), 1, "{failures:?}");
    let errors = &failures[0].errors;
    assert_eq!(errors.len(), 2, "{}", failures[0]);
    assert!(
        matches!(&errors[0], LabError::Io { source, .. } if source.kind() == std::io::ErrorKind::TimedOut)
    );
    assert!(
        matches!(&errors[1], LabError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(config.store.base.join("f1").exists());
    kept.teardown().await.unwrap();
    assert_lab_removed(&sandbox, &config, &["f1"]);
}

/// Leftovers of a lab whose rollback failed are only removable once the blocker is gone.
async fn recover(sandbox: &Sandbox, config: &LabConfig, ids: &[&str]) {
    sandbox.release_busy_jails();
    for id in ids {
        // The failed lab may still hold its lock while its drop-time cleanup finishes.
        support::wait_until_async("lab lock released", Duration::from_secs(5), || async {
            Lab::teardown_by_name(config, id).await.is_ok()
        })
        .await;
    }
    for dir in ["jails/firecracker", "cgroup/firecracker"] {
        let _ = fs::remove_dir_all(sandbox.dir.path().join(dir));
    }
    for id in ids {
        assert!(!config.store.base.join(id).exists());
    }
}

#[tokio::test]
async fn failed_up_rollback_is_reported_as_unrecovered() {
    if !sandboxed("failed_up_rollback_is_reported_as_unrecovered") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabPartialLaunch);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    sandbox.make_teardown_fail();

    let error = Lab::up_with(&config, &spec, "lab").await.err().unwrap();

    assert!(
        matches!(error, LabError::Rollback { .. }) && error.leaves_resources(),
        "{error:?}"
    );
    assert!(config.store.base.join("lab").exists());
    recover(&sandbox, &config, &["lab"]).await;
}

/// Forks two clones; the first one cannot even be created, so its error is the first returned.
async fn fork_with_blocked_first_clone(
    sandbox: &Sandbox,
    config: &LabConfig,
    spec: &LabSpec,
) -> (LabError, [String; 2]) {
    let mut source = Lab::up_with(config, spec, "src").await.unwrap();
    source.snapshot("saved").await.unwrap();
    source.teardown().await.unwrap();
    let snapshot = config.store.snapshot_dir("src", "saved");
    let ids = ["x1".to_string(), "x2".to_string()];
    fs::create_dir_all(config.store.base.join(&ids[0])).unwrap();
    sandbox.make_teardown_fail();

    let error = Lab::fork_with(config, &snapshot, &ids).await.err().unwrap();
    (error, ids)
}

#[tokio::test]
async fn fork_reports_a_later_clone_whose_rollback_failed() {
    if !sandboxed("fork_reports_a_later_clone_whose_rollback_failed") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }
    fs::write(sandbox.dir.path().join("fail-restore"), "load").unwrap();

    let (error, ids) = fork_with_blocked_first_clone(&sandbox, &config, &spec).await;

    assert!(
        matches!(&error, LabError::Rollback { operation, .. } if !operation.leaves_resources()),
        "{error:?}"
    );
    fs::remove_dir_all(config.store.base.join(&ids[0])).unwrap();
    recover(&sandbox, &config, &[&ids[1]]).await;
}

#[tokio::test]
async fn fork_reports_a_successful_clone_that_could_not_be_torn_down() {
    if !sandboxed("fork_reports_a_successful_clone_that_could_not_be_torn_down") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(&config, &spec).unwrap() {
        return;
    }

    let (error, ids) = fork_with_blocked_first_clone(&sandbox, &config, &spec).await;

    assert!(error.leaves_resources(), "{error:?}");
    fs::remove_dir_all(config.store.base.join(&ids[0])).unwrap();
    recover(&sandbox, &config, &[&ids[1]]).await;
}

#[test]
fn default_jails_follow_store_parent_and_keep_socket_paths_short() {
    use std::path::Path;
    assert_eq!(
        LabConfig::default().jailer.chroot_base_dir,
        Path::new("target/jails")
    );
    assert_eq!(
        LabConfig::default_jail_dir(Path::new("/data/labs")),
        Path::new("/data/jails")
    );
    assert_eq!(
        LabConfig::default_jail_dir(Path::new("labs")),
        Path::new("jails")
    );
    let config = LabConfig::default();
    let paths = dylos_runtime::jailer::JailPaths::new(&config.jailer, "lab32-abc", "A").unwrap();
    assert!(paths.api_socket().as_os_str().len() < 108);
}

#[tokio::test]
async fn preflight_rejects_bad_image_before_creating_resources() {
    let (sandbox, mut config, spec) = setup(Mode::Serve);
    config.store.image = sandbox.dir.path().join("missing-image");
    let error = Lab::up_with(&config, &spec, "lab").await.err().unwrap();
    let message = error.to_string();
    assert!(message.contains("reflink preflight"), "{message}");
    assert!(
        message.contains(config.store.image.to_str().unwrap()),
        "{message}"
    );
    assert!(
        message.contains(config.store.base.to_str().unwrap()),
        "{message}"
    );
    assert!(!config.store.base.exists());
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn restore_and_fork_reject_unpublished_staging() {
    let (_sandbox, config, _spec) = setup(Mode::LabReady);
    let staging = config.store.stage_snapshot("source", "snap").unwrap();
    for result in [
        Lab::restore_with(&config, staging.root(), "restored")
            .await
            .map(|_| ()),
        Lab::fork(&config, staging.root(), "forked", 2)
            .await
            .map(|_| ()),
    ] {
        let error = result.unwrap_err();
        assert!(
            matches!(error, LabError::Store(dylos_store::Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::InvalidInput)
        );
    }
    assert!(!config.store.base.join("restored").exists());
    assert!(!config.store.base.join("forked-c1").exists());
    assert!(!config.store.base.join("forked-c2").exists());
}

#[tokio::test]
async fn condition_polling_backs_off_until_success() {
    let start = std::time::Instant::now();
    let mut polls = 0;
    support::wait_until("backoff", Duration::from_secs(5), || {
        polls += 1;
        start.elapsed() >= Duration::from_millis(35)
    })
    .await;
    assert!(polls <= 5, "condition was polled {polls} times");

    let start = std::time::Instant::now();
    let mut polls = 0;
    support::wait_until_async("async backoff", Duration::from_secs(5), || {
        polls += 1;
        std::future::ready(start.elapsed() >= Duration::from_millis(35))
    })
    .await;
    assert!(polls <= 5, "async condition was polled {polls} times");
}

#[tokio::test]
async fn condition_polling_still_times_out() {
    let task = tokio::spawn(support::wait_until(
        "never ready",
        Duration::from_millis(25),
        || false,
    ));
    assert!(task.await.unwrap_err().is_panic());
    let task = tokio::spawn(support::wait_until_async(
        "never ready asynchronously",
        Duration::from_millis(25),
        || std::future::ready(false),
    ));
    assert!(task.await.unwrap_err().is_panic());
}

#[tokio::test]
async fn up_rejects_control_socket_exceeding_sun_len_before_creating_resources() {
    let (sandbox, mut config, spec) = setup(Mode::Serve);
    let long_base = sandbox.dir.path().join("s".repeat(95));
    config.store.base = long_base.clone();
    let err = Lab::up_with(&config, &spec, "lab").await.err().unwrap();
    match &err {
        LabError::SocketPathTooLong {
            lab,
            path,
            len,
            limit,
        } => {
            assert_eq!(lab, "lab");
            assert_eq!(*limit, dylos_runtime::MAX_UNIX_SOCKET_PATH);
            assert_eq!(*len, path.as_os_str().len());
            assert!(*len > *limit, "expected len {len} > limit {limit}");
            assert_eq!(path, &Lab::control_socket_path(&long_base, "lab"));
            let msg = err.to_string();
            assert!(msg.contains(&len.to_string()), "{msg}");
            assert!(msg.contains(&limit.to_string()), "{msg}");
            assert!(msg.contains("lab"), "{msg}");
        }
        other => panic!("expected SocketPathTooLong, got {other:?}"),
    }
    assert!(!long_base.exists(), "store base should not be created");
    assert_removed(&sandbox, &config);
}

#[tokio::test]
async fn up_rejects_api_socket_exceeding_sun_len_before_creating_resources() {
    let (sandbox, mut config, spec) = setup(Mode::Serve);
    let long_jail_dir = sandbox.dir.path().join("j".repeat(80));
    config.jailer.chroot_base_dir = long_jail_dir.clone();
    let err = Lab::up_with(&config, &spec, "lab").await.err().unwrap();
    match &err {
        LabError::Vm(dylos_runtime::Error::SocketPathTooLong { path, len, limit }) => {
            assert_eq!(*limit, dylos_runtime::MAX_UNIX_SOCKET_PATH);
            assert_eq!(*len, path.as_os_str().len());
            assert!(*len > *limit, "expected len {len} > limit {limit}");
            assert!(path.ends_with("run/firecracker.socket"));
            let msg = err.to_string();
            assert!(msg.contains(&len.to_string()), "{msg}");
            assert!(msg.contains(&limit.to_string()), "{msg}");
        }
        other => panic!("expected Vm(SocketPathTooLong), got {other:?}"),
    }
    assert!(!long_jail_dir.exists(), "jail dir should not be created");
    assert!(
        !config.store.base.exists(),
        "store base should not be created"
    );
    assert_removed(&sandbox, &config);
}

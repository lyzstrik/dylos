#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::process::Command;
use std::time::Duration;

use dylos_core::LabSpec;
use dylos_runtime::{Lab, LabConfig, RestorePool, Timeouts};
use support::{Mode, Sandbox};

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

fn supported(test: &str, config: &LabConfig, spec: &LabSpec) -> bool {
    match config.store.create("probe", spec) {
        Ok(mut lab) => {
            lab.remove().unwrap();
            true
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
            eprintln!("SKIPPED {test}: reflinks unavailable: {source}");
            false
        }
        Err(error) => panic!("store probe failed: {error}"),
    }
}

#[tokio::test]
async fn pool_prewarms_and_hands_out_instantly_with_background_refill() {
    if !sandboxed("pool_prewarms_and_hands_out_instantly_with_background_refill") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(
        "pool_prewarms_and_hands_out_instantly_with_background_refill",
        &config,
        &spec,
    ) {
        return;
    }
    let mut source = Lab::up_with(&config, &spec, "src").await.unwrap();
    source.snapshot("saved").await.unwrap();
    let snapshot = config.store.snapshot_dir("src", "saved");

    let mut pool = RestorePool::new(&config, &snapshot, "pool", 2)
        .await
        .unwrap();
    assert_eq!(pool.capacity(), 2);
    assert_eq!(pool.ready_count(), 2);

    let mut clone1 = pool.take().await.unwrap();
    assert!(clone1.id() == "pool-p1" || clone1.id() == "pool-p2");
    assert!(clone1.restore_latency().is_some());
    for vm in clone1.vms() {
        let requests = fs::read_to_string(vm.paths().root.join("requests")).unwrap();
        assert!(requests.contains("PUT /snapshot/load "));
        assert!(requests.contains("Resumed"));
    }

    let mut clone2 = pool.take().await.unwrap();
    assert!(clone2.id() == "pool-p1" || clone2.id() == "pool-p2");
    assert_ne!(clone1.id(), clone2.id());
    assert!(clone2.restore_latency().is_some());

    clone1.teardown().await.unwrap();
    clone2.teardown().await.unwrap();
    pool.teardown().await.unwrap();
    source.teardown().await.unwrap();

    sandbox.assert_no_jail_left();
    assert!(!config.store.base.join("pool-p1").exists());
    assert!(!config.store.base.join("pool-p2").exists());
}

#[tokio::test]
async fn pool_capacity_validation_and_clean_teardown() {
    if !sandboxed("pool_capacity_validation_and_clean_teardown") {
        return;
    }
    let (sandbox, config, spec) = setup(Mode::LabReady);
    if !supported(
        "pool_capacity_validation_and_clean_teardown",
        &config,
        &spec,
    ) {
        return;
    }
    let mut source = Lab::up_with(&config, &spec, "src2").await.unwrap();
    source.snapshot("saved2").await.unwrap();
    let snapshot = config.store.snapshot_dir("src2", "saved2");

    assert!(
        RestorePool::new(&config, &snapshot, "pool2", 0)
            .await
            .is_err()
    );
    assert!(
        RestorePool::new(&config, &snapshot, "pool2", 100)
            .await
            .is_err()
    );

    let mut pool = RestorePool::new(&config, &snapshot, "pool2", 2)
        .await
        .unwrap();
    assert_eq!(pool.ready_count(), 2);
    pool.teardown().await.unwrap();
    source.teardown().await.unwrap();

    sandbox.assert_no_jail_left();
    assert!(!config.store.base.join("pool2-p1").exists());
    assert!(!config.store.base.join("pool2-p2").exists());
}

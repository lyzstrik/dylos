#![allow(clippy::unwrap_used, clippy::expect_used)]

use dylos_core::LabSpec;
use dylos_runtime::{LabConfig, jailer::JailPaths};
use nix::fcntl::{Flock, FlockArg};
use std::fs::{self, File};

#[test]
fn lists_every_state_without_modifying_resources() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = LabConfig::default();
    config.store.base = temp.path().join("labs");
    config.netns_dir = temp.path().join("netns");
    config.jailer.chroot_base_dir = temp.path().join("custom-jails");
    let proc_dir = temp.path().join("proc");
    fs::create_dir_all(&proc_dir).unwrap();
    fs::create_dir_all(&config.netns_dir).unwrap();
    let spec = LabSpec::from_yaml_str(include_str!("../../dylos-runtime/tests/fixtures/lab.yaml"))
        .unwrap();
    let mut locks = Vec::new();
    for (index, (id, supervisor, namespace, processes)) in [
        ("running", true, true, true),
        ("stopped", false, false, false),
        ("abandoned", false, true, true),
        ("owner-only", true, false, false),
        ("namespace-only", false, true, false),
        ("process-only", false, false, true),
        ("missing-vms", true, true, false),
    ]
    .into_iter()
    .enumerate()
    {
        let lab = config.store.base.join(id);
        fs::create_dir_all(&lab).unwrap();
        let state = serde_json::to_vec(&serde_json::json!({"lab_id":id,"spec":spec})).unwrap();
        fs::write(lab.join("state.json"), &state).unwrap();
        if supervisor {
            locks.push(
                Flock::lock(File::open(&lab).unwrap(), FlockArg::LockExclusiveNonblock).unwrap(),
            );
        }
        if namespace {
            fs::write(config.netns_dir.join(format!("dylos-{id}")), "").unwrap();
        }
        if processes {
            for (vm, node) in spec.nodes.iter().enumerate() {
                let process = proc_dir.join((index * 10 + vm + 1).to_string());
                fs::create_dir(&process).unwrap();
                let jail =
                    JailPaths::new(&config.jailer, &format!("lab{}-{id}", id.len()), &node.name)
                        .unwrap();
                let group = jail
                    .cgroup_dir
                    .strip_prefix(&config.jailer.cgroup_root)
                    .unwrap();
                fs::write(process.join("cgroup"), format!("0::/{}\n", group.display())).unwrap();
            }
        }
    }
    fs::create_dir(config.store.base.join("snapshots")).unwrap();
    let labs = dylos_cli::list_labs(&config, &proc_dir).unwrap();
    assert_eq!(labs.len(), 7);
    for lab in labs {
        let expected = match lab.id.as_str() {
            "running" => "running",
            "stopped" => "stopped",
            _ => "orphaned",
        };
        assert_eq!(lab.state, expected, "{}", lab.id);
        assert_eq!(lab.vms, spec.nodes.len());
        assert_eq!(lab.directory, config.store.base.join(&lab.id));
        let expected =
            serde_json::to_vec(&serde_json::json!({"lab_id":lab.id,"spec":spec})).unwrap();
        assert_eq!(
            fs::read(lab.directory.join("state.json")).unwrap(),
            expected
        );
        assert_eq!(fs::read_dir(&lab.directory).unwrap().count(), 1);
    }
    // Lock probes never release another owner's lock.
    for lock in &locks {
        assert!(
            Flock::lock(
                File::open(format!(
                    "/proc/self/fd/{}",
                    std::os::fd::AsRawFd::as_raw_fd(&**lock)
                ))
                .unwrap(),
                FlockArg::LockSharedNonblock
            )
            .is_err()
        );
    }
}

#[test]
fn missing_store_is_empty_and_remains_absent() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = LabConfig::default();
    config.store.base = temp.path().join("absent");
    assert!(
        dylos_cli::list_labs(&config, temp.path())
            .unwrap()
            .is_empty()
    );
    assert!(!config.store.base.exists());
}

#[test]
fn cli_respects_store_override_without_creating_jails() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("custom-store");
    let jail = temp.path().join("custom-jails");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dylos"))
        .env("DYLOS_LAB_DIR", &store)
        .env("DYLOS_JAIL_DIR", &jail)
        .args(["lab", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, [] as [u8; 0]);
    assert!(!store.exists());
    assert!(!jail.exists());
}

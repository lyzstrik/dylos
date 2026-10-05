use std::ffi::OsString;
use std::path::Path;

use dylos_runtime::Error;
use dylos_runtime::jailer::{
    ChrootFile, JailPaths, JailerConfig, jailer_args, prepare_chroot, remove_jail,
};

fn config() -> JailerConfig {
    let mut config = JailerConfig::new("/srv/dylos", 1234, 5678);
    config.cgroups = vec!["cpu.max=50000 100000".into()];
    config
}

#[allow(clippy::unwrap_used)]
fn strings(args: &[OsString]) -> Vec<&str> {
    args.iter().map(|a| a.to_str().unwrap()).collect()
}

#[test]
fn jail_id_and_paths_are_derived_from_lab_and_node() {
    let paths = JailPaths::new(&config(), "lab-7", "router").unwrap();
    assert_eq!(paths.id, "lab-7-router");
    assert_eq!(
        paths.jail_dir,
        Path::new("/srv/dylos/firecracker/lab-7-router")
    );
    assert_eq!(
        paths.root,
        Path::new("/srv/dylos/firecracker/lab-7-router/root")
    );
    assert_eq!(
        paths.api_socket(),
        Path::new("/srv/dylos/firecracker/lab-7-router/root/run/firecracker.socket")
    );
    assert_eq!(
        paths.cgroup_dir,
        Path::new("/sys/fs/cgroup/firecracker/lab-7-router")
    );
}

#[test]
fn ids_the_jailer_rejects_are_refused() {
    for (lab, node) in [
        ("lab_1", "a"),
        ("lab", "a/b"),
        ("lab", "é"),
        ("l", &"n".repeat(63)),
    ] {
        let err = JailPaths::new(&config(), lab, node).unwrap_err();
        assert!(
            matches!(err, Error::InvalidJailId { .. }),
            "{lab}-{node}: {err}"
        );
    }
    assert!(JailPaths::new(&config(), "l", &"n".repeat(62)).is_ok());
}

#[test]
fn command_line_uses_jail_relative_paths_and_drops_privileges() {
    let config = config();
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let netns = Path::new("/run/netns/dylos-lab");
    let args = jailer_args(&config, &paths, Some(netns));
    assert_eq!(
        strings(&args).join(" "),
        "--id lab-web --exec-file /usr/bin/firecracker --uid 1234 --gid 5678 \
         --chroot-base-dir /srv/dylos --cgroup-version 2 --cgroup cpu.max=50000 100000 \
         --netns /run/netns/dylos-lab \
         -- --api-sock /run/firecracker.socket --metrics-path /run/metrics.json"
    );
}

#[test]
fn command_line_is_identical_for_every_clone_and_has_no_netns_when_none() {
    let mut other_host = config();
    other_host.parent_cgroup = Some("dylos".into());
    let a = jailer_args(
        &config(),
        &JailPaths::new(&config(), "lab", "db").unwrap(),
        None,
    );
    let b = jailer_args(
        &config(),
        &JailPaths::new(&config(), "lab", "db").unwrap(),
        None,
    );
    assert_eq!(a, b);
    assert!(!strings(&a).contains(&"--netns"));
    let c = jailer_args(
        &other_host,
        &JailPaths::new(&other_host, "lab", "db").unwrap(),
        None,
    );
    assert!(
        strings(&c)
            .windows(2)
            .any(|w| w == ["--parent-cgroup", "dylos"])
    );
    assert_eq!(
        ChrootFile::new("/x/rootfs-db.ext4", "rootfs.ext4").jailed_path(),
        "/rootfs.ext4"
    );
}

#[allow(clippy::unwrap_used)]
fn local_config(dir: &Path) -> JailerConfig {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir).unwrap();
    JailerConfig::new(dir.join("jails"), meta.uid(), meta.gid())
}

#[tokio::test]
async fn chroot_is_populated_then_removed_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    let source = dir.path().join("vmlinux.bin");
    std::fs::write(&source, b"kernel").unwrap();
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    prepare_chroot(&config, &paths, &[ChrootFile::new(&source, "vmlinux.bin")])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(paths.root.join("vmlinux.bin")).unwrap(),
        b"kernel"
    );
    assert!(paths.root.join("run").is_dir());
    assert!(paths.root.join("run/metrics.json").is_file());

    remove_jail(&paths).await.unwrap();
    remove_jail(&paths).await.unwrap();
    assert!(!paths.jail_dir.exists());
    assert!(source.exists());
}

#[tokio::test]
async fn failed_preparation_rolls_back_and_bad_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let missing = ChrootFile::new(dir.path().join("missing"), "rootfs.ext4");
    let err = prepare_chroot(&config, &paths, &[missing])
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Io {
                op: "hard link",
                ..
            }
        ),
        "{err}"
    );
    assert!(!paths.jail_dir.exists());

    for name in ["", ".", "..", "run", "a/b"] {
        let file = ChrootFile::new(dir.path().join("x"), name);
        let err = prepare_chroot(&config, &paths, &[file]).await.unwrap_err();
        assert!(
            matches!(err, Error::InvalidChrootFileName { .. }),
            "{name:?}: {err}"
        );
    }
    assert!(!paths.jail_dir.exists());
}

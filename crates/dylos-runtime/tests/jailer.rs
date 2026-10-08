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
    let mut jail = prepare_chroot(&config, &paths, &[ChrootFile::new(&source, "vmlinux.bin")])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(paths.root.join("vmlinux.bin")).unwrap(),
        b"kernel"
    );
    assert!(paths.root.join("run").is_dir());
    assert!(paths.root.join("run/metrics.json").is_file());

    jail.release().await.unwrap();
    remove_jail(&paths).await.unwrap();
    assert!(!paths.jail_dir.exists());
    assert!(source.exists());
}

#[tokio::test]
async fn releasing_a_released_jail_leaves_its_replacement_intact() {
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    let source = dir.path().join("vmlinux.bin");
    std::fs::write(&source, b"kernel").unwrap();
    let files = [ChrootFile::new(&source, "vmlinux.bin")];
    let paths = JailPaths::new(&config, "lab", "web").unwrap();

    let mut first = prepare_chroot(&config, &paths, &files).await.unwrap();
    first.release().await.unwrap();
    let mut second = prepare_chroot(&config, &paths, &files).await.unwrap();

    first.release().await.unwrap();
    assert!(paths.root.join("vmlinux.bin").is_file());
    assert!(paths.root.join("run/metrics.json").is_file());

    second.release().await.unwrap();
    assert!(!paths.jail_dir.exists());
}

#[test]
fn per_vm_cgroup_is_requested_even_without_properties() {
    let config = JailerConfig::new("/srv/dylos", 1234, 5678);
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let args = jailer_args(&config, &paths, None);
    let cgroups: Vec<&str> = strings(&args)
        .windows(2)
        .filter(|w| w[0] == "--cgroup")
        .map(|w| w[1])
        .collect();
    assert_eq!(cgroups, ["pids.max=max"]);
    assert_eq!(
        strings(&jailer_args(&self::config(), &paths, None))
            .iter()
            .filter(|a| **a == "--cgroup")
            .count(),
        1
    );
}

#[tokio::test]
async fn concurrent_claims_have_one_winner_and_losers_touch_nothing() {
    // Each preparation runs on its own blocking thread, so the claims really race.
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    let source = dir.path().join("vmlinux.bin");
    std::fs::write(&source, b"kernel").unwrap();
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let files = [ChrootFile::new(&source, "vmlinux.bin")];
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let (config, paths, files) = (config.clone(), paths.clone(), files.clone());
            tokio::spawn(async move { prepare_chroot(&config, &paths, &files).await })
        })
        .collect();
    let mut winners = Vec::new();
    for attempt in attempts {
        match attempt.await.unwrap() {
            Ok(jail) => winners.push(jail),
            Err(err) => assert!(matches!(err, Error::JailExists { .. }), "{err}"),
        }
    }
    assert_eq!(winners.len(), 1);
    assert!(paths.root.join("vmlinux.bin").is_file());
    assert!(paths.root.join("run/metrics.json").is_file());
    winners[0].release().await.unwrap();
    assert!(!paths.jail_dir.exists());
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
                op: "validate source",
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

#[tokio::test]
async fn symlink_and_non_regular_sources_are_rejected_without_touching_target() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let target = dir.path().join("outside");
    std::fs::write(&target, b"outside contents").unwrap();
    let before = std::fs::symlink_metadata(&target).unwrap();
    let source = dir.path().join("symlink");
    symlink(&target, &source).unwrap();
    for source in [&source, &dir.path().to_path_buf()] {
        let err = prepare_chroot(&config, &paths, &[ChrootFile::new(source, "kernel")])
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Io { id, op: "validate regular source", path, .. }
            if id == &paths.id && path == source),
            "{err}"
        );
        assert!(!paths.jail_dir.exists());
    }
    let after = std::fs::symlink_metadata(&target).unwrap();
    assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    assert_eq!(std::fs::read(&target).unwrap(), b"outside contents");
}

#[tokio::test]
async fn source_symlink_replacement_after_validation_is_rejected() {
    source_replacement_is_rejected(false, false).await;
}

#[tokio::test]
async fn source_ancestor_symlink_replacement_after_validation_is_rejected() {
    source_replacement_is_rejected(true, false).await;
}

#[tokio::test]
async fn source_symlink_replacement_after_open_is_not_followed_by_placement() {
    source_replacement_is_rejected(false, true).await;
}

#[allow(clippy::unwrap_used)]
async fn source_replacement_is_rejected(ancestor: bool, after_open: bool) {
    use std::os::unix::fs::{MetadataExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let mut config = local_config(dir.path());
    let parent = dir.path().join("sources");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let source = parent.join("kernel");
    let target = outside.join("kernel");
    std::fs::write(&source, b"kernel").unwrap();
    std::fs::write(&target, b"outside contents").unwrap();
    let before = std::fs::symlink_metadata(&target).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    config.source_validation_barrier = Some(barrier.clone());
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let task_paths = paths.clone();
    let task_source = source.clone();
    let task = tokio::spawn(async move {
        prepare_chroot(
            &config,
            &task_paths,
            &[ChrootFile::new(task_source, "kernel")],
        )
        .await
    });
    let race_target = target.clone();
    let _dir = tokio::task::spawn_blocking(move || {
        barrier.wait();
        if after_open {
            barrier.wait();
            barrier.wait();
        }
        if ancestor {
            std::fs::rename(&parent, dir.path().join("original-sources")).unwrap();
            symlink(&outside, &parent).unwrap();
        } else {
            std::fs::remove_file(&source).unwrap();
            symlink(&race_target, &source).unwrap();
        }
        barrier.wait();
        dir
    })
    .await
    .unwrap();
    let err = task.await.unwrap().unwrap_err();
    assert!(
        matches!(
            err,
            Error::Io {
                op: "open source" | "open source parent" | "open placed file",
                ..
            }
        ),
        "{err}"
    );
    assert!(!paths.jail_dir.exists());
    let after = std::fs::symlink_metadata(&target).unwrap();
    assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    assert_eq!(std::fs::read(&target).unwrap(), b"outside contents");
}

#[tokio::test]
async fn injected_jail_directory_symlink_is_rejected_without_touching_target() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let mut config = local_config(dir.path());
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let target = outside.join("metrics.json");
    std::fs::write(&target, b"outside contents").unwrap();
    let before = std::fs::symlink_metadata(&outside).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    config.preparation_barrier = Some(barrier.clone());
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let task_paths = paths.clone();
    let task = tokio::spawn(async move { prepare_chroot(&config, &task_paths, &[]).await });
    let root = paths.root.clone();
    let outside_copy = outside.clone();
    tokio::task::spawn_blocking(move || {
        barrier.wait();
        symlink(outside_copy, root).unwrap();
        barrier.wait();
    })
    .await
    .unwrap();
    let err = task.await.unwrap().unwrap_err();
    assert!(
        matches!(
            err,
            Error::Io {
                op: "create dir",
                ..
            }
        ),
        "{err}"
    );
    assert!(!paths.jail_dir.exists());
    let after = std::fs::symlink_metadata(&outside).unwrap();
    assert_eq!((before.uid(), before.gid()), (after.uid(), after.gid()));
    assert_eq!(std::fs::read(&target).unwrap(), b"outside contents");
    assert!(!outside.join("run").exists());
}

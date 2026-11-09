mod support;

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dylos_runtime::Error;
use dylos_runtime::jailer::{
    ChrootFile, JailPaths, JailerConfig, MAX_UNIX_SOCKET_PATH, check_static_firecracker,
    check_static_firecracker_sync, has_elf_interpreter, jailer_args, prepare_chroot, remove_jail,
    validate_unix_socket_path,
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
    let max_id_config = JailerConfig::new("/j", 1234, 5678);
    assert!(JailPaths::new(&max_id_config, "l", &"n".repeat(62)).is_ok());
}

#[test]
fn command_line_uses_jail_relative_paths_and_drops_privileges() {
    let config = config();
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let netns = Path::new("/run/netns/dylos-lab");
    let args = jailer_args(&config, &paths, Some(netns), 1, 128);
    assert_eq!(
        strings(&args).join(" "),
        "--id lab-web --exec-file /usr/bin/firecracker --uid 1234 --gid 5678 \
         --chroot-base-dir /srv/dylos --cgroup-version 2 \
         --cgroup pids.max=33 --cgroup memory.max=268435456 --cgroup cpu.max=50000 100000 \
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
        1,
        128,
    );
    let b = jailer_args(
        &config(),
        &JailPaths::new(&config(), "lab", "db").unwrap(),
        None,
        1,
        128,
    );
    assert_eq!(a, b);
    assert!(!strings(&a).contains(&"--netns"));
    let c = jailer_args(
        &other_host,
        &JailPaths::new(&other_host, "lab", "db").unwrap(),
        None,
        1,
        128,
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
fn per_vm_cgroup_has_bounded_default_resources() {
    let config = JailerConfig::new("/srv/dylos", 1234, 5678);
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    let args = jailer_args(&config, &paths, None, 1, 128);
    let cgroups: Vec<&str> = strings(&args)
        .windows(2)
        .filter(|w| w[0] == "--cgroup")
        .map(|w| w[1])
        .collect();
    assert_eq!(cgroups, ["pids.max=33", "memory.max=268435456"]);
    assert_eq!(
        strings(&jailer_args(&self::config(), &paths, None, 1, 128))
            .iter()
            .filter(|a| **a == "--cgroup")
            .count(),
        3
    );
}

#[test]
fn default_limits_scale_with_vm_resources_without_overflow() {
    let config = JailerConfig::new("/srv/dylos", 1234, 5678);
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    for (vcpus, mib, pids, bytes) in [
        (8, 512, 40, 640 * 1024 * 1024),
        (
            u8::MAX,
            u32::MAX,
            287,
            (u64::from(u32::MAX) + 128) * 1024 * 1024,
        ),
    ] {
        let args = jailer_args(&config, &paths, None, vcpus, mib);
        let args = strings(&args);
        assert!(
            args.windows(2)
                .any(|w| w == ["--cgroup", &format!("pids.max={pids}")])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--cgroup", &format!("memory.max={bytes}")])
        );
    }
}

#[test]
fn limits_are_independently_overridable_and_explicit_cgroups_win() {
    let mut config = JailerConfig::new("/srv/dylos", 1234, 5678);
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    for (pids, memory, expected) in [
        (Some(64), None, ["pids.max=64", "memory.max=268435456"]),
        (
            None,
            Some(536_870_912),
            ["pids.max=33", "memory.max=536870912"],
        ),
        (
            Some(64),
            Some(536_870_912),
            ["pids.max=64", "memory.max=536870912"],
        ),
    ] {
        config.pids_max = pids;
        config.memory_max = memory;
        let args = jailer_args(&config, &paths, None, 1, 128);
        let values: Vec<_> = strings(&args)
            .windows(2)
            .filter(|w| w[0] == "--cgroup")
            .map(|w| w[1])
            .collect();
        assert_eq!(values, expected);
    }
    config.cgroups = vec!["pids.max=99".into(), "memory.max=1073741824".into()];
    let args = jailer_args(&config, &paths, None, 1, 128);
    let values: Vec<_> = strings(&args)
        .windows(2)
        .filter(|w| w[0] == "--cgroup")
        .map(|w| w[1])
        .collect();
    assert_eq!(values, ["pids.max=99", "memory.max=1073741824"]);
}

#[test]
fn explicit_cgroups_override_only_matching_properties_once() {
    let mut config = JailerConfig::new("/srv/dylos", 1234, 5678);
    let paths = JailPaths::new(&config, "lab", "web").unwrap();
    for (explicit, expected) in [
        (
            vec!["cpu.weight=50"],
            vec!["pids.max=33", "memory.max=268435456", "cpu.weight=50"],
        ),
        (
            vec!["pids.max=99"],
            vec!["memory.max=268435456", "pids.max=99"],
        ),
        (
            vec!["memory.max=1073741824"],
            vec!["pids.max=33", "memory.max=1073741824"],
        ),
        (
            vec!["pids.max=max"],
            vec!["memory.max=268435456", "pids.max=max"],
        ),
        (
            vec!["memory.max=max"],
            vec!["pids.max=33", "memory.max=max"],
        ),
        (
            vec![
                "pids.max=99",
                "pids.max=max",
                "cpu.weight=20",
                "cpu.weight=50",
            ],
            vec!["memory.max=268435456", "pids.max=max", "cpu.weight=50"],
        ),
    ] {
        config.cgroups = explicit.into_iter().map(String::from).collect();
        let args = jailer_args(&config, &paths, None, 1, 128);
        let values: Vec<_> = strings(&args)
            .windows(2)
            .filter(|w| w[0] == "--cgroup")
            .map(|w| w[1])
            .collect();
        assert_eq!(values, expected);
        let properties: std::collections::BTreeSet<_> = values
            .iter()
            .map(|value| value.split_once('=').unwrap().0)
            .collect();
        assert_eq!(properties.len(), values.len());
    }
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
async fn read_only_file_is_shared_unchanged_and_writable_source_is_refused() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    let source = dir.path().join("vm.mem");
    std::fs::write(&source, b"memory").unwrap();
    let paths = JailPaths::new(&config, "lab", "web").unwrap();

    let err = prepare_chroot(&config, &paths, &[ChrootFile::read_only(&source, "vm.mem")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Io { op: "validate read-only source", source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied),
        "{err:?}"
    );
    assert!(!paths.jail_dir.exists());

    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
    let before = std::fs::metadata(&source).unwrap();
    let mut jail = prepare_chroot(&config, &paths, &[ChrootFile::read_only(&source, "vm.mem")])
        .await
        .unwrap();
    let placed = std::fs::metadata(paths.root.join("vm.mem")).unwrap();
    assert_eq!((placed.dev(), placed.ino()), (before.dev(), before.ino()));
    let after = std::fs::metadata(&source).unwrap();
    assert_eq!(after.mode(), before.mode());
    assert_eq!(after.uid(), before.uid());
    jail.release().await.unwrap();
    remove_jail(&paths).await.unwrap();
}

#[test]
fn jail_paths_rejects_api_socket_exceeding_sun_len() {
    let long_base = PathBuf::from("/tmp").join("a".repeat(80));
    let config = JailerConfig::new(long_base, 65534, 65534);
    let err = JailPaths::new(&config, "lab-id", "node-a").unwrap_err();
    let Error::SocketPathTooLong { path, len, limit } = &err else {
        unreachable!("expected SocketPathTooLong, got {err:?}");
    };
    assert_eq!(*limit, dylos_runtime::MAX_UNIX_SOCKET_PATH);
    assert_eq!(*len, path.as_os_str().len());
    assert!(*len > *limit, "expected len {len} > limit {limit}");
    assert!(path.ends_with("run/firecracker.socket"));
    let msg = err.to_string();
    assert!(msg.contains(&len.to_string()), "{msg}");
    assert!(msg.contains(&limit.to_string()), "{msg}");
    assert!(msg.contains(path.to_str().unwrap()), "{msg}");
}

#[test]
fn jail_paths_boundary_exact_107_accepted_and_108_rejected() {
    // Suffix after chroot_base_dir:
    // /firecracker/l-n/root/run/firecracker.socket
    // Length: 1 + 11 + 1 + 3 + 1 + 4 + 1 + 3 + 1 + 18 = 44 bytes.
    // Exact 107-byte acceptance: base length = 107 - 44 = 63 bytes.
    let base_107 = PathBuf::from(format!("/{}", "a".repeat(62)));
    assert_eq!(base_107.as_os_str().as_bytes().len(), 63);
    let config_107 = JailerConfig::new(base_107, 65534, 65534);
    let paths_107 = JailPaths::new(&config_107, "l", "n").unwrap();
    let socket_107 = paths_107.api_socket();
    assert_eq!(socket_107.as_os_str().as_bytes().len(), 107);
    assert_eq!(socket_107.as_os_str().len(), 107);
    assert!(validate_unix_socket_path(&socket_107).is_ok());

    // Exact 108-byte rejection: base length = 108 - 44 = 64 bytes.
    let base_108 = PathBuf::from(format!("/{}", "a".repeat(63)));
    assert_eq!(base_108.as_os_str().as_bytes().len(), 64);
    let config_108 = JailerConfig::new(base_108, 65534, 65534);
    let Error::SocketPathTooLong { path, len, limit } =
        JailPaths::new(&config_108, "l", "n").unwrap_err()
    else {
        unreachable!("expected SocketPathTooLong");
    };
    assert_eq!(len, 108);
    assert_eq!(limit, MAX_UNIX_SOCKET_PATH);
    assert_eq!(len, path.as_os_str().as_bytes().len());
    assert_eq!(limit, 107);
    assert_eq!(
        path,
        config_108
            .chroot_base_dir
            .join("firecracker/l-n/root/run/firecracker.socket")
    );
}

#[test]
fn jail_paths_multibyte_utf8_counts_bytes_not_characters() {
    // Each '🦀' is 4 bytes in UTF-8, but only 1 char.
    // 3 crabs = 12 bytes, 3 chars (byte count - char count = +9).
    // Total byte length desired: 108 bytes.
    // Suffix: 44 bytes. Base: 64 bytes.
    // Base layout: '/' (1 byte) + 51 'a's (51 bytes) + 3 crabs (12 bytes) = 64 bytes.
    // Character count of base: 1 + 51 + 3 = 55 chars.
    // Total character count of socket: 55 + 44 = 99 chars <= 107 chars!
    let base_multibyte_108 = PathBuf::from(format!("/{}🦀🦀🦀", "a".repeat(51)));
    assert_eq!(base_multibyte_108.as_os_str().as_bytes().len(), 64);
    assert_eq!(base_multibyte_108.to_str().unwrap().chars().count(), 55);

    let config_108 = JailerConfig::new(base_multibyte_108, 65534, 65534);
    let Error::SocketPathTooLong { len, limit, path } =
        JailPaths::new(&config_108, "l", "n").unwrap_err()
    else {
        unreachable!("expected SocketPathTooLong");
    };
    assert_eq!(len, 108);
    assert_eq!(limit, 107);
    // Verify that character count would have erroneously passed an off-by-one or char-count check
    assert_eq!(path.to_str().unwrap().chars().count(), 99);
    assert_eq!(path.as_os_str().as_bytes().len(), 108);

    // Exact 107-byte acceptance with multibyte UTF-8:
    // Base: '/' (1 byte) + 50 'a's (50 bytes) + 3 crabs (12 bytes) = 63 bytes.
    // Total byte length: 63 + 44 = 107 bytes.
    let base_multibyte_107 = PathBuf::from(format!("/{}🦀🦀🦀", "a".repeat(50)));
    assert_eq!(base_multibyte_107.as_os_str().as_bytes().len(), 63);
    let config_107 = JailerConfig::new(base_multibyte_107, 65534, 65534);
    let paths_107 = JailPaths::new(&config_107, "l", "n").unwrap();
    let socket = paths_107.api_socket();
    assert_eq!(socket.as_os_str().as_bytes().len(), 107);
    assert_eq!(socket.to_str().unwrap().chars().count(), 98);
    assert!(validate_unix_socket_path(&socket).is_ok());
}

#[test]
fn jail_paths_non_utf8_counts_raw_bytes_without_panicking() {
    // Non-UTF-8 bytes containing invalid sequences:
    let mut bytes_108 = b"/non_utf8_".to_vec();
    bytes_108.extend_from_slice(&[0xff, 0xfe, 0xfd]);
    while bytes_108.len() < 64 {
        bytes_108.push(b'x');
    }
    assert_eq!(bytes_108.len(), 64);
    let base_non_utf8_108 = PathBuf::from(OsString::from_vec(bytes_108));
    let config_108 = JailerConfig::new(base_non_utf8_108, 65534, 65534);
    let Error::SocketPathTooLong { len, limit, path } =
        JailPaths::new(&config_108, "l", "n").unwrap_err()
    else {
        unreachable!("expected SocketPathTooLong");
    };
    assert_eq!(len, 108);
    assert_eq!(limit, 107);
    assert_eq!(path.as_os_str().as_bytes().len(), 108);

    // Non-UTF-8 107-byte acceptance:
    let mut bytes_107 = b"/non_utf8_".to_vec();
    bytes_107.extend_from_slice(&[0xff, 0xfe, 0xfd]);
    while bytes_107.len() < 63 {
        bytes_107.push(b'x');
    }
    assert_eq!(bytes_107.len(), 63);
    let base_non_utf8_107 = PathBuf::from(OsString::from_vec(bytes_107));
    let config_107 = JailerConfig::new(base_non_utf8_107, 65534, 65534);
    let paths_107 = JailPaths::new(&config_107, "l", "n").unwrap();
    let socket = paths_107.api_socket();
    assert_eq!(socket.as_os_str().as_bytes().len(), 107);
    assert!(validate_unix_socket_path(&socket).is_ok());
}

#[test]
fn deep_working_directory_cross_filesystem_fallback_keeps_socket_paths_within_sun_len() {
    let parent = tempfile::tempdir().unwrap();
    let deep_ws = parent
        .path()
        .join("d".repeat(35))
        .join("e".repeat(35))
        .join("p".repeat(35))
        .join("workspace");
    std::fs::create_dir_all(&deep_ws).unwrap();
    let ws_canonical = deep_ws.canonicalize().unwrap();
    assert!(
        ws_canonical.as_os_str().len() > 107,
        "deep workspace path {:?} must exceed 107 bytes (got {})",
        ws_canonical,
        ws_canonical.as_os_str().len()
    );

    // Force the cross-filesystem fallback by passing a tmp directory on a different filesystem (/proc)
    let fallback_tempdir = support::test_tempdir_with_tmp(&deep_ws, Path::new("/proc"));

    let deep_dev = std::fs::metadata(&deep_ws).unwrap().dev();
    let temp_dev = std::fs::metadata(fallback_tempdir.path()).unwrap().dev();
    assert_eq!(
        deep_dev, temp_dev,
        "fallback tempdir must reside on the same filesystem mount as the deep checkout"
    );

    assert!(
        !fallback_tempdir.path().starts_with(&deep_ws),
        "fallback tempdir {:?} must not embed the deep checkout path {:?}",
        fallback_tempdir.path(),
        deep_ws
    );

    let config = JailerConfig::new(fallback_tempdir.path().join("j"), 65534, 65534);
    let paths = JailPaths::new(&config, "lab-id", "node-a").unwrap();
    let socket = paths.api_socket();
    assert!(
        socket.as_os_str().len() <= MAX_UNIX_SOCKET_PATH,
        "socket path length {} exceeds limit {}",
        socket.as_os_str().len(),
        MAX_UNIX_SOCKET_PATH
    );
    assert!(validate_unix_socket_path(&socket).is_ok());

    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket);
    assert!(
        listener.is_ok(),
        "failed to bind socket at {socket:?}: {listener:?}"
    );
    drop(listener);
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn static_binary_fixture_is_accepted_as_static_elf() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/static_binary");
    assert!(fixture.exists(), "fixture missing at {fixture:?}");
    assert!(!has_elf_interpreter(&fixture).unwrap());
    assert!(check_static_firecracker(&fixture).await.is_ok());
    assert!(check_static_firecracker_sync(&fixture).is_ok());
}

#[tokio::test]
async fn static_binary_elf32_fixture_is_accepted() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/static_binary_elf32");
    assert!(fixture.exists(), "fixture missing at {fixture:?}");
    assert!(!has_elf_interpreter(&fixture).unwrap());
    assert!(check_static_firecracker(&fixture).await.is_ok());
}

#[tokio::test]
async fn static_binary_big_endian_fixture_is_accepted() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/elf/static_binary_big_endian");
    assert!(fixture.exists(), "fixture missing at {fixture:?}");
    assert!(!has_elf_interpreter(&fixture).unwrap());
    assert!(check_static_firecracker(&fixture).await.is_ok());
}

#[tokio::test]
async fn static_binary_pie_fixture_is_accepted() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/static_binary_pie");
    assert!(fixture.exists(), "fixture missing at {fixture:?}");
    assert!(!has_elf_interpreter(&fixture).unwrap());
    assert!(check_static_firecracker(&fixture).await.is_ok());
}

#[tokio::test]
async fn dynamic_binary_fixture_is_rejected_with_dynamic_firecracker_error() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/dynamic_binary");
    assert!(fixture.exists(), "fixture missing at {fixture:?}");
    assert!(has_elf_interpreter(&fixture).unwrap());

    let err = check_static_firecracker(&fixture).await.unwrap_err();
    assert!(
        matches!(&err, Error::DynamicFirecracker { path } if path == &fixture),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("dynamically linked"), "{msg}");
    assert!(msg.contains("DYLOS_FC_BIN_DIR"), "{msg}");
    assert!(msg.contains("docs/host.md"), "{msg}");
    assert!(msg.contains(&fixture.display().to_string()), "{msg}");
}

#[tokio::test]
async fn non_elf_file_is_rejected_as_invalid_firecracker() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp.path(), b"not an elf file\n").unwrap();
    assert!(has_elf_interpreter(temp.path()).is_err());
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("magic")),
        "{err:?}"
    );
}

#[tokio::test]
async fn directory_path_is_rejected_as_invalid_firecracker() {
    let temp = tempfile::tempdir().unwrap();
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("regular file")),
        "{err:?}"
    );
}

#[tokio::test]
async fn truncated_elf_header_is_rejected() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp.path(), b"\x7fELF\x02\x01\x01\x00truncated").unwrap();
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("truncated")),
        "{err:?}"
    );
}

#[tokio::test]
async fn unsupported_elf_class_is_rejected() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = include_bytes!("fixtures/elf/static_binary").to_vec();
    bytes[4] = 3;
    std::fs::write(temp.path(), &bytes).unwrap();
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("class")),
        "{err:?}"
    );
}

#[tokio::test]
async fn unsupported_elf_endianness_is_rejected() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = include_bytes!("fixtures/elf/static_binary").to_vec();
    bytes[5] = 0;
    std::fs::write(temp.path(), &bytes).unwrap();
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("encoding")),
        "{err:?}"
    );
}

#[tokio::test]
async fn program_header_table_extending_beyond_eof_is_rejected() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = include_bytes!("fixtures/elf/static_binary").to_vec();
    bytes[0x38] = 10;
    std::fs::write(temp.path(), &bytes).unwrap();
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("beyond file length")),
        "{err:?}"
    );
}

#[tokio::test]
async fn undersized_phentsize_is_rejected() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = include_bytes!("fixtures/elf/static_binary").to_vec();
    bytes[0x36] = 16;
    std::fs::write(temp.path(), &bytes).unwrap();
    let err = check_static_firecracker(temp.path()).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains("entry size")),
        "{err:?}"
    );
}

#[tokio::test]
async fn check_static_firecracker_fails_on_missing_file() {
    let missing = PathBuf::from("/nonexistent/firecracker/binary");
    let err = check_static_firecracker(&missing).await.unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err:?}");
}

#[tokio::test]
async fn malformed_program_header_layouts_fail_closed() {
    for (fixture, phoff, ehsize, phentsize, phnum, header_size, entry_size) in [
        (
            include_bytes!("fixtures/elf/dynamic_binary").as_slice(),
            32,
            52,
            54,
            56,
            64u16,
            56u16,
        ),
        (
            include_bytes!("fixtures/elf/static_binary_elf32").as_slice(),
            28,
            40,
            42,
            44,
            52u16,
            32u16,
        ),
    ] {
        for (field, value, reason) in [
            (phoff, 0u64, "program header table"),
            (phoff, u64::from(header_size - 1), "program header table"),
            (phoff, 4096, "beyond file length"),
            (phoff, u64::MAX, "overflow"),
            (phnum, 0, "program header table"),
            (phnum, 0xffff, "program header table"),
            (phnum, 100, "beyond file length"),
            (phentsize, 0, "entry size"),
            (phentsize, u64::from(entry_size - 1), "entry size"),
            (phentsize, u64::from(entry_size + 1), "entry size"),
            (ehsize, u64::from(header_size - 1), "header size"),
            (ehsize, u64::from(header_size + 1), "header size"),
        ] {
            let mut bytes = fixture.to_vec();
            let width = if field == phoff {
                if header_size == 64 { 8 } else { 4 }
            } else {
                2
            };
            bytes[field..field + width].copy_from_slice(&value.to_le_bytes()[..width]);
            let temp = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(temp.path(), bytes).unwrap();
            let err = check_static_firecracker(temp.path()).await.unwrap_err();
            let expected = if header_size == 52 && value == u64::MAX {
                "beyond file length"
            } else {
                reason
            };
            assert!(
                matches!(&err, Error::InvalidFirecrackerBinary { path, reason } if path == temp.path() && reason.contains(expected)),
                "ELF{header_size} field {field}, value {value}: {err:?}"
            );
        }
    }
}

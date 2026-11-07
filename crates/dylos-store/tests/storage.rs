use dylos_core::LabSpec;
use dylos_store::{Error, State, Store};
use std::{
    fs,
    os::unix::{
        fs::{MetadataExt, symlink},
        net::UnixListener,
    },
};

fn setup() -> Result<(tempfile::TempDir, Store, LabSpec), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let store = Store {
        base: temp.path().join("labs"),
        image: temp.path().join("base.ext4"),
    };
    fs::write(&store.image, b"immutable image")?;
    let spec = LabSpec::from_yaml_str(include_str!("fixtures/lab.yaml"))?;
    Ok((temp, store, spec))
}
fn supported(test: &str, store: &Store, spec: &LabSpec) -> Result<bool, Error> {
    match store.create("probe", spec) {
        Ok(mut lab) => {
            lab.remove()?;
            Ok(true)
        }
        Err(Error::Reflink { source, .. })
            if matches!(
                source.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL)
            ) =>
        {
            eprintln!("SKIPPED {test}: reflinks unsupported on CARGO_TARGET_TMPDIR: {source}");
            assert!(!store.base.join("probe").exists());
            Ok(false)
        }
        Err(error) => Err(error),
    }
}
#[test]
fn layout_state_reflinks_exclusivity_and_raii() {
    let (_temp, store, spec) = setup().unwrap();
    if !supported("layout_state_reflinks_exclusivity_and_raii", &store, &spec).unwrap() {
        return;
    }
    let mut lab = store.create("lab-1", &spec).unwrap();
    assert_eq!(lab.root(), store.base.join("lab-1"));
    let state: State = serde_json::from_slice(&fs::read(lab.state_path()).unwrap()).unwrap();
    assert_eq!(state.lab_id, "lab-1");
    assert_eq!(state.spec, spec);
    assert!(state.created_at <= std::time::SystemTime::now());
    assert_eq!(fs::metadata(lab.root()).unwrap().mode() & 0o777, 0o700);
    let a = lab.vm_paths("a").unwrap();
    let b = lab.vm_paths("b").unwrap();
    assert_eq!(a.directory, lab.root().join("vms/a"));
    assert_eq!(a.rootfs, a.directory.join("rootfs.ext4"));
    assert_eq!(a.control_socket, a.directory.join("control.sock"));
    assert!(!a.control_socket.exists());
    assert_ne!(
        fs::metadata(&a.rootfs).unwrap().ino(),
        fs::metadata(&store.image).unwrap().ino()
    );
    assert_eq!(fs::read(&a.rootfs).unwrap(), b"immutable image");
    fs::write(&a.rootfs, b"vm changes").unwrap();
    assert_eq!(fs::read(&b.rootfs).unwrap(), b"immutable image");
    assert_eq!(fs::read(&store.image).unwrap(), b"immutable image");
    assert!(store.create("lab-1", &spec).is_err());
    assert_eq!(fs::read(&a.rootfs).unwrap(), b"vm changes");
    let socket = UnixListener::bind(&a.control_socket).unwrap();
    lab.remove().unwrap();
    lab.remove().unwrap();
    drop(socket);
    let replacement = store.create("lab-1", &spec).unwrap();
    drop(lab);
    assert!(replacement.root().exists());
    drop(replacement);
    store.remove("lab-1").unwrap();
    assert_eq!(fs::read_dir(&store.base).unwrap().count(), 0);
}
#[test]
fn traversal_invalid_specs_and_source_types_are_rejected_and_rolled_back() {
    let (temp, mut store, mut spec) = setup().unwrap();
    for id in ["", ".", "..", "../escape", "a/b", "a_b", &"a".repeat(33)] {
        assert!(store.create(id, &spec).is_err());
        assert!(store.remove(id).is_err());
    }
    spec.nodes[0].name = "../escape".into();
    assert!(store.create("lab", &spec).is_err());
    assert!(!store.base.exists());
    spec.nodes[0].name = "a".into();
    let source = store.image.clone();
    store.image = temp.path().join("missing");
    assert!(store.create("lab", &spec).is_err());
    assert!(!store.base.join("lab").exists());
    symlink(&source, &store.image).unwrap();
    assert!(store.create("lab", &spec).is_err());
    fs::remove_file(&store.image).unwrap();
    fs::create_dir(&store.image).unwrap();
    assert!(store.create("lab", &spec).is_err());
    fs::remove_dir(&store.image).unwrap();
    nix::unistd::mkfifo(&store.image, nix::sys::stat::Mode::S_IRUSR).unwrap();
    assert!(store.create("lab", &spec).is_err());
    assert_eq!(fs::read_dir(&store.base).unwrap().count(), 0);
}
#[test]
fn symlink_ancestors_and_existing_labs_cannot_redirect_storage() {
    let (temp, mut store, spec) = setup().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"keep").unwrap();
    symlink(&outside, &store.base).unwrap();
    assert!(store.create("lab", &spec).is_err());
    assert!(store.remove("lab").is_err());
    store.base = store.base.join("child");
    assert!(store.create("lab", &spec).is_err());
    assert!(!outside.join("child").exists());
    store.base = temp.path().join("real");
    fs::create_dir(&store.base).unwrap();
    symlink(&outside, store.base.join("lab")).unwrap();
    assert!(store.create("lab", &spec).is_err());
    store.remove("lab").unwrap();
    let image_link = temp.path().join("image-link");
    symlink(temp.path(), &image_link).unwrap();
    store.image = image_link.join("base.ext4");
    assert!(store.create("lab", &spec).is_err());
    assert!(!store.base.join("lab").exists());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"keep");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
}
#[test]
fn partial_removal_unlinks_nested_symlinks_and_is_idempotent() {
    let (temp, store, _spec) = setup().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"keep").unwrap();
    fs::create_dir_all(store.base.join("partial/vms/a")).unwrap();
    symlink(&outside, store.base.join("partial/vms/a/rootfs.ext4")).unwrap();
    symlink(&outside, store.base.join("partial/vms/b")).unwrap();
    store.remove("partial").unwrap();
    store.remove("partial").unwrap();
    assert_eq!(fs::read_dir(&store.base).unwrap().count(), 0);
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"keep");
}
#[test]
fn owned_handle_refuses_replacement_directory() {
    let (_temp, store, spec) = setup().unwrap();
    if !supported("owned_handle_refuses_replacement_directory", &store, &spec).unwrap() {
        return;
    }
    let mut lab = store.create("lab", &spec).unwrap();
    let moved = store.base.join("moved");
    fs::rename(lab.root(), &moved).unwrap();
    let replacement = store.create("lab", &spec).unwrap();
    assert!(lab.remove().is_err());
    drop(lab);
    assert!(replacement.state_path().exists());
    store.remove("moved").unwrap();
    drop(replacement);
    assert_eq!(fs::read_dir(&store.base).unwrap().count(), 0);
}
#[test]
fn unsupported_reflink_never_falls_back_to_copy() {
    let (_temp, mut store, spec) = setup().unwrap();
    let Ok(other) = tempfile::tempdir_in("/dev/shm") else {
        eprintln!("SKIPPED unsupported_reflink_never_falls_back_to_copy: /dev/shm unavailable");
        return;
    };
    store.base = other.path().join("labs");
    assert!(matches!(
        store.create("lab", &spec),
        Err(Error::Reflink { .. })
    ));
    assert!(!store.base.join("lab").exists());
    assert_eq!(fs::read(&store.image).unwrap(), b"immutable image");
}

#[test]
fn preflight_probes_real_clones_without_leaving_files_or_directories() {
    let (temp, store, spec) = setup().unwrap();
    if !supported(
        "preflight_probes_real_clones_without_leaving_files_or_directories",
        &store,
        &spec,
    )
    .unwrap()
    {
        return;
    }
    fs::remove_dir(&store.base).unwrap();
    let before: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    store.preflight(&temp.path().join("jails")).unwrap();
    let after: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
    assert!(!store.base.exists());
}

#[test]
fn preflight_rejects_jail_on_an_incompatible_mount_and_cleans_up() {
    let (temp, store, spec) = setup().unwrap();
    if !supported(
        "preflight_rejects_jail_on_an_incompatible_mount_and_cleans_up",
        &store,
        &spec,
    )
    .unwrap()
    {
        return;
    }
    fs::remove_dir(&store.base).unwrap();
    // tmpfs cannot FICLONE; only this test-owned directory is touched.
    let jail = tempfile::tempdir_in("/dev/shm").unwrap();
    let error = store.preflight(jail.path()).unwrap_err();
    assert!(matches!(error, Error::ReflinkPreflight { .. }));
    let message = error.to_string();
    assert!(message.contains(jail.path().to_str().unwrap()), "{message}");
    assert!(message.contains(store.base.to_str().unwrap()), "{message}");
    assert_eq!(fs::read_dir(jail.path()).unwrap().count(), 0);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn preflight_rejects_image_on_an_incompatible_mount_before_creation() {
    let (temp, mut store, _spec) = setup().unwrap();
    let image = tempfile::tempdir_in("/dev/shm").unwrap();
    store.image = image.path().join("rootfs.ext4");
    fs::write(&store.image, b"rootfs").unwrap();
    let error = store.preflight(&temp.path().join("jails")).unwrap_err();
    let message = error.to_string();
    assert!(message.contains(store.image.to_str().unwrap()), "{message}");
    assert!(message.contains(store.base.to_str().unwrap()), "{message}");
    assert!(!store.base.exists());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(image.path()).unwrap().count(), 1);
}

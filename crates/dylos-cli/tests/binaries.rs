#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use dylos_cli::{
    SecurityContext, candidate_search_dirs_with, check_binary_security, check_dir_security,
    resolve_firecracker_binaries_with, validate_binary_path,
};

fn test_temp_dir() -> tempfile::TempDir {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("binary_tests");
    fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}

fn make_executable(path: &Path) {
    File::create(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn current_security_context() -> SecurityContext {
    SecurityContext {
        orchestrator_uid: nix::unistd::geteuid().as_raw(),
        invoking_uid: nix::unistd::geteuid().as_raw(),
        invoking_gid: nix::unistd::getegid().as_raw(),
    }
}

#[test]
fn candidate_search_dirs_order_under_sudo() {
    let dirs = candidate_search_dirs_with(Some("lyzi"), Some(Path::new("/root")));
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/home/lyzi/.local/share/dylos/bin"),
            PathBuf::from("/root/.local/share/dylos/bin"),
            PathBuf::from("/usr/local/lib/dylos/bin"),
            PathBuf::from("/opt/dylos/bin"),
        ]
    );
}

#[test]
fn candidate_search_dirs_order_without_sudo() {
    let dirs = candidate_search_dirs_with(None, Some(Path::new("/home/lyzi")));
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/home/lyzi/.local/share/dylos/bin"),
            PathBuf::from("/usr/local/lib/dylos/bin"),
            PathBuf::from("/opt/dylos/bin"),
        ]
    );
}

#[test]
fn candidate_search_dirs_deduplicate() {
    let dirs = candidate_search_dirs_with(Some("lyzi"), Some(Path::new("/home/lyzi")));
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/home/lyzi/.local/share/dylos/bin"),
            PathBuf::from("/usr/local/lib/dylos/bin"),
            PathBuf::from("/opt/dylos/bin"),
        ]
    );
}

#[test]
fn env_var_overrides_everything() {
    let temp = test_temp_dir();
    let override_dir = temp.path().join("override");
    let candidate_dir = temp.path().join("candidate");
    fs::create_dir(&override_dir).unwrap();
    fs::create_dir(&candidate_dir).unwrap();

    make_executable(&override_dir.join("firecracker"));
    make_executable(&override_dir.join("jailer"));
    make_executable(&candidate_dir.join("firecracker"));
    make_executable(&candidate_dir.join("jailer"));

    let ctx = current_security_context();
    let resolved = resolve_firecracker_binaries_with(
        Some(&override_dir),
        &ctx,
        std::slice::from_ref(&candidate_dir),
    )
    .unwrap();

    assert_eq!(resolved.bin_dir, override_dir);
    assert_eq!(resolved.firecracker, override_dir.join("firecracker"));
    assert_eq!(resolved.jailer, override_dir.join("jailer"));
}

#[test]
fn resolution_order_selects_first_complete_candidate() {
    let temp = test_temp_dir();
    let candidate1 = temp.path().join("cand1");
    let candidate2 = temp.path().join("cand2");
    fs::create_dir(&candidate1).unwrap();
    fs::create_dir(&candidate2).unwrap();

    // candidate1 is incomplete (only firecracker)
    make_executable(&candidate1.join("firecracker"));

    // candidate2 is complete (both firecracker and jailer)
    make_executable(&candidate2.join("firecracker"));
    make_executable(&candidate2.join("jailer"));

    let ctx = current_security_context();
    let resolved =
        resolve_firecracker_binaries_with(None, &ctx, &[candidate1, candidate2.clone()]).unwrap();

    assert_eq!(resolved.bin_dir, candidate2);
    assert_eq!(resolved.firecracker, candidate2.join("firecracker"));
    assert_eq!(resolved.jailer, candidate2.join("jailer"));
}

#[test]
fn missing_binaries_reports_all_tried_locations_and_docs() {
    let temp = test_temp_dir();
    let dir1 = temp.path().join("dir1");
    let dir2 = temp.path().join("dir2");

    let ctx = current_security_context();
    let err =
        resolve_firecracker_binaries_with(None, &ctx, &[dir1.clone(), dir2.clone()]).unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("Firecracker and jailer binaries not found"),
        "{msg}"
    );
    assert!(msg.contains(&dir1.display().to_string()), "{msg}");
    assert!(msg.contains(&dir2.display().to_string()), "{msg}");
    assert!(msg.contains("docs/host.md"), "{msg}");
    assert!(msg.contains("DYLOS_FC_BIN_DIR"), "{msg}");
}

#[test]
fn permission_check_accepts_valid_binary() {
    let temp = test_temp_dir();
    let bin = temp.path().join("firecracker");
    make_executable(&bin);

    let ctx = current_security_context();
    let canonical = validate_binary_path(&bin, &ctx).unwrap();
    assert_eq!(canonical, bin.canonicalize().unwrap());
}

#[test]
fn permission_check_rejects_world_writable_binary() {
    let temp = test_temp_dir();
    let bin = temp.path().join("firecracker");
    File::create(&bin).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o777)).unwrap();

    let ctx = current_security_context();
    let err = validate_binary_path(&bin, &ctx).unwrap_err();
    assert!(err.to_string().contains("world-writable"), "{err}");
}

#[test]
fn permission_check_rejects_world_writable_parent_directory() {
    let temp = test_temp_dir();
    let bad_dir = temp.path().join("bad_parent");
    fs::create_dir(&bad_dir).unwrap();
    fs::set_permissions(&bad_dir, fs::Permissions::from_mode(0o777)).unwrap();

    let bin = bad_dir.join("firecracker");
    make_executable(&bin);

    let ctx = current_security_context();
    let err = validate_binary_path(&bin, &ctx).unwrap_err();
    assert!(err.to_string().contains("world-writable"), "{err}");
}

#[test]
fn permission_check_rejects_non_executable_binary() {
    let temp = test_temp_dir();
    let bin = temp.path().join("firecracker");
    File::create(&bin).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();

    let ctx = current_security_context();
    let err = validate_binary_path(&bin, &ctx).unwrap_err();
    assert!(err.to_string().contains("not executable"), "{err}");
}

#[test]
fn permission_check_rejects_untrusted_binary_owner() {
    let temp = test_temp_dir();
    let bin = temp.path().join("firecracker");
    make_executable(&bin);

    let ctx = SecurityContext {
        orchestrator_uid: 0,
        invoking_uid: 99999, // untrusted uid
        invoking_gid: 99999,
    };
    let err = check_binary_security(&bin, &ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("only trusts root (uid 0) and invoking owner"),
        "{msg}"
    );
}

#[test]
fn permission_check_rejects_untrusted_parent_directory_owner() {
    let temp = test_temp_dir();
    let ctx = SecurityContext {
        orchestrator_uid: 0,
        invoking_uid: 99999, // untrusted uid
        invoking_gid: 99999,
    };
    let err = check_dir_security(temp.path(), &ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("only trusts root (uid 0) and invoking owner"),
        "{msg}"
    );
}

#[test]
fn permission_check_rejects_group_writable_by_untrusted_gid() {
    let temp = test_temp_dir();
    let sub_dir = temp.path().join("grp_dir");
    fs::create_dir(&sub_dir).unwrap();
    fs::set_permissions(&sub_dir, fs::Permissions::from_mode(0o775)).unwrap();

    let ctx = SecurityContext {
        orchestrator_uid: nix::unistd::geteuid().as_raw(),
        invoking_uid: nix::unistd::geteuid().as_raw(),
        invoking_gid: 99999, // different from real file gid
    };
    let err = check_dir_security(&sub_dir, &ctx).unwrap_err();
    assert!(
        err.to_string().contains("group-writable by untrusted gid"),
        "{err}"
    );
}

#[test]
fn permission_check_rejects_non_regular_file() {
    let temp = test_temp_dir();
    let ctx = current_security_context();
    let err = check_binary_security(temp.path(), &ctx).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
}

#[test]
fn permission_check_rejects_untrusted_symlink() {
    let temp = test_temp_dir();
    let target = temp.path().join("real_bin");
    make_executable(&target);
    let link = temp.path().join("link_bin");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let ctx = SecurityContext {
        orchestrator_uid: 0,
        invoking_uid: 99999,
        invoking_gid: 99999,
    };
    let err = validate_binary_path(&link, &ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("only trusts root (uid 0) and invoking owner"),
        "{err}"
    );
}

#[test]
fn resolution_fails_closed_when_candidate_fails_security() {
    let temp = test_temp_dir();
    let candidate = temp.path().join("insecure_cand");
    fs::create_dir(&candidate).unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o777)).unwrap();

    make_executable(&candidate.join("firecracker"));
    make_executable(&candidate.join("jailer"));

    let ctx = current_security_context();
    let err = resolve_firecracker_binaries_with(None, &ctx, std::slice::from_ref(&candidate))
        .unwrap_err();
    assert!(err.to_string().contains("world-writable"), "{err}");
}

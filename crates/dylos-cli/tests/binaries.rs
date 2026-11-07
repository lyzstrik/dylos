#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use dylos_cli::binaries::{
    PasswdUser, SecurityContext, candidate_search_dirs_with, check_binary_security,
    check_dir_security, resolve_firecracker_binaries_with, resolve_sudo_identity_with,
    validate_binary_path,
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
        invoking_uid: nix::unistd::geteuid().as_raw(),
    }
}

fn fake_passwd_lookup(name: &str) -> Option<PasswdUser> {
    if name == "alice" {
        Some(PasswdUser {
            name: "alice".into(),
            uid: 1001,
            gid: 1001,
            dir: PathBuf::from("/home/alice"),
        })
    } else {
        None
    }
}

#[test]
fn candidate_search_dirs_order_under_sudo() {
    let dirs = candidate_search_dirs_with(
        Some(Path::new("/home/invoking_user")),
        Some(Path::new("/root")),
    );
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/home/invoking_user/.local/share/dylos/bin"),
            PathBuf::from("/root/.local/share/dylos/bin"),
            PathBuf::from("/usr/local/lib/dylos/bin"),
            PathBuf::from("/opt/dylos/bin"),
        ]
    );
}

#[test]
fn candidate_search_dirs_order_without_sudo() {
    let dirs = candidate_search_dirs_with(None, Some(Path::new("/home/user")));
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/home/user/.local/share/dylos/bin"),
            PathBuf::from("/usr/local/lib/dylos/bin"),
            PathBuf::from("/opt/dylos/bin"),
        ]
    );
}

#[test]
fn candidate_search_dirs_deduplicate() {
    let dirs =
        candidate_search_dirs_with(Some(Path::new("/home/user")), Some(Path::new("/home/user")));
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/home/user/.local/share/dylos/bin"),
            PathBuf::from("/usr/local/lib/dylos/bin"),
            PathBuf::from("/opt/dylos/bin"),
        ]
    );
}

#[test]
fn sudo_identity_resolves_when_consistent() {
    let user = resolve_sudo_identity_with(
        0,
        Some("alice"),
        Some("1001"),
        Some("1001"),
        fake_passwd_lookup,
    );
    assert_eq!(
        user,
        Some(PasswdUser {
            name: "alice".into(),
            uid: 1001,
            gid: 1001,
            dir: PathBuf::from("/home/alice"),
        })
    );
}

#[test]
fn sudo_identity_resolves_when_sudo_gid_absent() {
    let user = resolve_sudo_identity_with(0, Some("alice"), Some("1001"), None, fake_passwd_lookup);
    assert_eq!(
        user,
        Some(PasswdUser {
            name: "alice".into(),
            uid: 1001,
            gid: 1001,
            dir: PathBuf::from("/home/alice"),
        })
    );
}

#[test]
fn sudo_identity_rejected_when_process_not_root() {
    let user = resolve_sudo_identity_with(
        1001,
        Some("alice"),
        Some("1001"),
        Some("1001"),
        fake_passwd_lookup,
    );
    assert_eq!(user, None);
}

#[test]
fn sudo_identity_rejected_when_user_unknown() {
    let user = resolve_sudo_identity_with(
        0,
        Some("unknown_user"),
        Some("1001"),
        None,
        fake_passwd_lookup,
    );
    assert_eq!(user, None);
}

#[test]
fn sudo_identity_rejected_when_uid_mismatched() {
    let user = resolve_sudo_identity_with(
        0,
        Some("alice"),
        Some("9999"), // does not match passwd uid 1001
        None,
        fake_passwd_lookup,
    );
    assert_eq!(user, None);
}

#[test]
fn sudo_identity_rejected_when_uid_unparsable() {
    let user = resolve_sudo_identity_with(
        0,
        Some("alice"),
        Some("not_a_number"),
        None,
        fake_passwd_lookup,
    );
    assert_eq!(user, None);
}

#[test]
fn sudo_identity_rejected_when_gid_mismatched() {
    let user = resolve_sudo_identity_with(
        0,
        Some("alice"),
        Some("1001"),
        Some("9999"), // does not match passwd gid 1001
        fake_passwd_lookup,
    );
    assert_eq!(user, None);
}

#[test]
fn security_context_from_resolved_trusts_root_only_on_inconsistent_identity() {
    let ctx = SecurityContext::from_resolved(0, None);
    assert_eq!(ctx.invoking_uid, 0);
}

#[test]
fn security_context_from_resolved_trusts_invoking_uid_when_consistent() {
    let user = PasswdUser {
        name: "alice".into(),
        uid: 1001,
        gid: 1001,
        dir: PathBuf::from("/home/alice"),
    };
    let ctx = SecurityContext::from_resolved(0, Some(&user));
    assert_eq!(ctx.invoking_uid, 1001);
}

#[test]
fn system_passwd_lookup_succeeds_for_current_user() {
    let current_uid = nix::unistd::geteuid();
    if let Ok(Some(current_user)) = nix::unistd::User::from_uid(current_uid) {
        let looked_up = PasswdUser::from_system(&current_user.name);
        assert!(looked_up.is_some());
        let looked_up = looked_up.unwrap();
        assert_eq!(looked_up.name, current_user.name);
        assert_eq!(looked_up.uid, current_user.uid.as_raw());
    }
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
fn explicit_override_fails_and_never_falls_back() {
    let temp = test_temp_dir();
    let bad_override = temp.path().join("bad_override");
    fs::create_dir(&bad_override).unwrap();
    make_executable(&bad_override.join("firecracker"));

    let candidate_dir = temp.path().join("candidate");
    fs::create_dir(&candidate_dir).unwrap();
    make_executable(&candidate_dir.join("firecracker"));
    make_executable(&candidate_dir.join("jailer"));

    let ctx = current_security_context();
    let err = resolve_firecracker_binaries_with(
        Some(&bad_override),
        &ctx,
        std::slice::from_ref(&candidate_dir),
    )
    .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("DYLOS_FC_BIN_DIR"), "{msg}");
    assert!(
        msg.contains("Both 'firecracker' and 'jailer' are required"),
        "{msg}"
    );
}

#[test]
fn explicit_override_insecure_fails_and_never_falls_back() {
    let temp = test_temp_dir();
    let insecure_override = temp.path().join("insecure_override");
    fs::create_dir(&insecure_override).unwrap();
    fs::set_permissions(&insecure_override, fs::Permissions::from_mode(0o777)).unwrap();
    make_executable(&insecure_override.join("firecracker"));
    make_executable(&insecure_override.join("jailer"));

    let candidate_dir = temp.path().join("candidate");
    fs::create_dir(&candidate_dir).unwrap();
    make_executable(&candidate_dir.join("firecracker"));
    make_executable(&candidate_dir.join("jailer"));

    let ctx = current_security_context();
    let err = resolve_firecracker_binaries_with(
        Some(&insecure_override),
        &ctx,
        std::slice::from_ref(&candidate_dir),
    )
    .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("world-writable"), "{msg}");
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
        invoking_uid: 99999, // untrusted uid
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
        invoking_uid: 99999, // untrusted uid
    };
    let err = check_dir_security(temp.path(), &ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("only trusts root (uid 0) and invoking owner"),
        "{msg}"
    );
}

#[test]
fn permission_check_rejects_group_writable_binary_even_if_group_matches_invoking_gid() {
    let temp = test_temp_dir();
    let bin = temp.path().join("firecracker");
    make_executable(&bin);
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o775)).unwrap();

    let real_gid = nix::unistd::getegid().as_raw();
    let ctx = SecurityContext {
        invoking_uid: nix::unistd::geteuid().as_raw(),
    };
    if real_gid != 0 {
        let err = check_binary_security(&bin, &ctx).unwrap_err();
        assert!(err.to_string().contains("group-writable"), "{err}");
    }
}

#[test]
fn permission_check_rejects_group_writable_parent_directory() {
    let temp = test_temp_dir();
    let sub_dir = temp.path().join("grp_dir");
    fs::create_dir(&sub_dir).unwrap();
    fs::set_permissions(&sub_dir, fs::Permissions::from_mode(0o775)).unwrap();

    let real_gid = nix::unistd::getegid().as_raw();
    let ctx = SecurityContext {
        invoking_uid: nix::unistd::geteuid().as_raw(),
    };
    if real_gid != 0 {
        let err = check_dir_security(&sub_dir, &ctx).unwrap_err();
        assert!(err.to_string().contains("group-writable"), "{err}");
    }
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
        invoking_uid: 99999,
    };
    let err = validate_binary_path(&link, &ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("only trusts root (uid 0) and invoking owner"),
        "{err}"
    );
}

#[test]
fn symlink_target_in_insecure_parent_is_rejected() {
    let temp = test_temp_dir();
    let bad_parent = temp.path().join("bad_parent_dir");
    fs::create_dir(&bad_parent).unwrap();
    fs::set_permissions(&bad_parent, fs::Permissions::from_mode(0o777)).unwrap();

    let target = bad_parent.join("firecracker");
    make_executable(&target);

    let link_dir = temp.path().join("safe_dir");
    fs::create_dir(&link_dir).unwrap();
    let link = link_dir.join("firecracker");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let ctx = current_security_context();
    let err = validate_binary_path(&link, &ctx).unwrap_err();
    assert!(err.to_string().contains("world-writable"), "{err}");
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

#[test]
fn symlinked_ancestor_directory_is_accepted_when_canonical_target_is_secure() {
    let temp = test_temp_dir();
    let real_parent = temp.path().join("real_parent");
    fs::create_dir(&real_parent).unwrap();
    fs::set_permissions(&real_parent, fs::Permissions::from_mode(0o755)).unwrap();

    let real_bin = real_parent.join("firecracker");
    make_executable(&real_bin);

    let symlink_parent = temp.path().join("symlink_parent");
    std::os::unix::fs::symlink(&real_parent, &symlink_parent).unwrap();

    let bin_through_symlink = symlink_parent.join("firecracker");
    let ctx = current_security_context();

    let canonical = validate_binary_path(&bin_through_symlink, &ctx).unwrap();
    assert_eq!(canonical, real_bin.canonicalize().unwrap());
}

#[test]
fn symlinked_ancestor_directory_with_insecure_target_is_rejected() {
    let temp = test_temp_dir();
    let bad_parent = temp.path().join("bad_parent");
    fs::create_dir(&bad_parent).unwrap();
    fs::set_permissions(&bad_parent, fs::Permissions::from_mode(0o777)).unwrap();

    let real_bin = bad_parent.join("firecracker");
    make_executable(&real_bin);

    let symlink_parent = temp.path().join("symlink_parent");
    std::os::unix::fs::symlink(&bad_parent, &symlink_parent).unwrap();

    let bin_through_symlink = symlink_parent.join("firecracker");
    let ctx = current_security_context();

    let err = validate_binary_path(&bin_through_symlink, &ctx).unwrap_err();
    assert!(err.to_string().contains("world-writable"), "{err}");
}

//! Resolution and security validation for Firecracker and Jailer binaries.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Resolved binary paths for Firecracker and Jailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinaries {
    pub bin_dir: PathBuf,
    pub firecracker: PathBuf,
    pub jailer: PathBuf,
}

/// Security context representing the effective and invoking user/group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityContext {
    pub orchestrator_uid: u32,
    pub invoking_uid: u32,
    pub invoking_gid: u32,
}

impl SecurityContext {
    /// Determines the security context for the current process.
    #[must_use]
    #[allow(clippy::similar_names)]
    pub fn current() -> Self {
        let current_uid = nix::unistd::geteuid().as_raw();
        let current_gid = nix::unistd::getegid().as_raw();
        if current_uid == 0 {
            if let Ok(sudo_user) = std::env::var("SUDO_USER")
                && !sudo_user.is_empty()
                && let Ok(Some(user)) = nix::unistd::User::from_name(&sudo_user)
            {
                let owner_uid = std::env::var("SUDO_UID")
                    .ok()
                    .and_then(|u| u.parse::<u32>().ok())
                    .unwrap_or_else(|| user.uid.as_raw());
                let owner_gid = std::env::var("SUDO_GID")
                    .ok()
                    .and_then(|g| g.parse::<u32>().ok())
                    .unwrap_or_else(|| user.gid.as_raw());
                return Self {
                    orchestrator_uid: current_uid,
                    invoking_uid: owner_uid,
                    invoking_gid: owner_gid,
                };
            }
            Self {
                orchestrator_uid: 0,
                invoking_uid: 0,
                invoking_gid: current_gid,
            }
        } else {
            Self {
                orchestrator_uid: current_uid,
                invoking_uid: current_uid,
                invoking_gid: current_gid,
            }
        }
    }
}

/// Checks that a directory is not writable by untrusted users.
///
/// # Errors
/// Returns an error if the directory metadata cannot be read, if the directory is owned by an
/// unprivileged user other than the invoking owner, or if it is world-writable or group-writable by
/// an untrusted group.
pub fn check_dir_security(dir: &Path, ctx: &SecurityContext) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .with_context(|| format!("failed to read metadata of directory '{}'", dir.display()))?;

    if meta.uid() != 0 && meta.uid() != ctx.invoking_uid {
        bail!(
            "parent directory '{}' of Firecracker binary is owned by uid {}, but orchestrator only trusts root (uid 0) and invoking owner (uid {})",
            dir.display(),
            meta.uid(),
            ctx.invoking_uid
        );
    }

    if meta.mode() & 0o002 != 0 {
        bail!(
            "parent directory '{}' of Firecracker binary is world-writable (mode {:o}); refusing to execute",
            dir.display(),
            meta.mode() & 0o7777
        );
    }

    if (meta.mode() & 0o020 != 0) && (meta.gid() != 0 && meta.gid() != ctx.invoking_gid) {
        bail!(
            "parent directory '{}' of Firecracker binary is group-writable by untrusted gid {} (mode {:o}); refusing to execute",
            dir.display(),
            meta.gid(),
            meta.mode() & 0o7777
        );
    }

    Ok(())
}

/// Checks that an executable binary is not writable or owned by untrusted users.
///
/// # Errors
/// Returns an error if the binary cannot be inspected, is not a regular executable file, is owned by
/// an unprivileged user other than the invoking owner, or is world-writable or group-writable by an
/// untrusted group.
pub fn check_binary_security(path: &Path, ctx: &SecurityContext) -> Result<()> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to read metadata of binary '{}'", path.display()))?;

    if !meta.file_type().is_file() {
        bail!(
            "Firecracker binary '{}' is not a regular file",
            path.display()
        );
    }

    if meta.mode() & 0o111 == 0 {
        bail!(
            "Firecracker binary '{}' is not executable (mode {:o})",
            path.display(),
            meta.mode() & 0o7777
        );
    }

    if meta.uid() != 0 && meta.uid() != ctx.invoking_uid {
        bail!(
            "Firecracker binary '{}' is owned by uid {}, but orchestrator only trusts root (uid 0) and invoking owner (uid {})",
            path.display(),
            meta.uid(),
            ctx.invoking_uid
        );
    }

    if meta.mode() & 0o002 != 0 {
        bail!(
            "Firecracker binary '{}' is world-writable (mode {:o}); refusing to execute",
            path.display(),
            meta.mode() & 0o7777
        );
    }

    if (meta.mode() & 0o020 != 0) && (meta.gid() != 0 && meta.gid() != ctx.invoking_gid) {
        bail!(
            "Firecracker binary '{}' is group-writable by untrusted gid {} (mode {:o}); refusing to execute",
            path.display(),
            meta.gid(),
            meta.mode() & 0o7777
        );
    }

    Ok(())
}

/// Validates binary path and all its ancestor directories against the security context.
///
/// # Errors
/// Returns an error if the binary or any ancestor directory fails the ownership or permission
/// checks, or if path canonicalization fails.
pub fn validate_binary_path(path: &Path, ctx: &SecurityContext) -> Result<PathBuf> {
    if !path.exists() {
        bail!("binary '{}' does not exist", path.display());
    }

    let symlink_meta = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect symlink metadata of '{}'", path.display()))?;
    if symlink_meta.file_type().is_symlink()
        && symlink_meta.uid() != 0
        && symlink_meta.uid() != ctx.invoking_uid
    {
        bail!(
            "symlink '{}' to Firecracker binary is owned by uid {}, but orchestrator only trusts root (uid 0) and invoking owner (uid {})",
            path.display(),
            symlink_meta.uid(),
            ctx.invoking_uid
        );
    }

    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        check_dir_security(parent, ctx)?;
        current = parent.to_path_buf();
    }

    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize binary path '{}'", path.display()))?;

    check_binary_security(&canonical, ctx)?;

    for ancestor in canonical.ancestors() {
        if ancestor == canonical {
            continue;
        }
        check_dir_security(ancestor, ctx)?;
    }

    Ok(canonical)
}

/// Returns the ordered candidate search directories for Firecracker binaries.
#[must_use]
pub fn candidate_search_dirs_with(sudo_user: Option<&str>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user_name) = sudo_user.filter(|s| !s.is_empty())
        && let Ok(Some(user)) = nix::unistd::User::from_name(user_name)
    {
        let user_bin = user.dir.join(".local/share/dylos/bin");
        if !dirs.contains(&user_bin) {
            dirs.push(user_bin);
        }
    }
    if let Some(h) = home {
        let home_bin = h.join(".local/share/dylos/bin");
        if !dirs.contains(&home_bin) {
            dirs.push(home_bin);
        }
    }
    let system_bin = PathBuf::from("/usr/local/lib/dylos/bin");
    if !dirs.contains(&system_bin) {
        dirs.push(system_bin);
    }
    let opt_bin = PathBuf::from("/opt/dylos/bin");
    if !dirs.contains(&opt_bin) {
        dirs.push(opt_bin);
    }
    dirs
}

/// Resolves Firecracker and Jailer binaries with explicit configuration, security context, and candidate directories.
///
/// # Errors
/// Returns an error if binaries are missing in the resolved or candidate directories, or if any
/// binary fails security validation.
pub fn resolve_firecracker_binaries_with(
    env_bin_dir: Option<&Path>,
    ctx: &SecurityContext,
    candidate_dirs: &[PathBuf],
) -> Result<ResolvedBinaries> {
    if let Some(bin_dir) = env_bin_dir {
        let fc = bin_dir.join("firecracker");
        let jailer = bin_dir.join("jailer");
        if !fc.exists() || !jailer.exists() {
            bail!(
                "Firecracker or jailer binary not found in DYLOS_FC_BIN_DIR ('{}'). \
                 Both 'firecracker' and 'jailer' are required. See docs/host.md",
                bin_dir.display()
            );
        }
        let fc = validate_binary_path(&fc, ctx)?;
        let jailer = validate_binary_path(&jailer, ctx)?;
        return Ok(ResolvedBinaries {
            bin_dir: bin_dir.to_path_buf(),
            firecracker: fc,
            jailer,
        });
    }

    let mut tried = Vec::new();
    for candidate in candidate_dirs {
        tried.push(candidate.clone());
        let fc = candidate.join("firecracker");
        let jailer = candidate.join("jailer");
        if fc.exists() && jailer.exists() {
            let fc = validate_binary_path(&fc, ctx)?;
            let jailer = validate_binary_path(&jailer, ctx)?;
            return Ok(ResolvedBinaries {
                bin_dir: candidate.clone(),
                firecracker: fc,
                jailer,
            });
        }
    }

    let mut msg = String::from("Firecracker and jailer binaries not found. Tried:\n");
    for dir in &tried {
        let _ = writeln!(msg, "  - {}", dir.display());
    }
    msg.push_str("Install the pinned static binaries (see docs/host.md) or set DYLOS_FC_BIN_DIR.");
    bail!("{msg}");
}

/// Resolves Firecracker and Jailer binaries using the process environment and security context.
///
/// # Errors
/// Returns an error if binaries are missing or fail security validation.
pub fn resolve_firecracker_binaries(env_bin_dir: Option<&Path>) -> Result<ResolvedBinaries> {
    let ctx = SecurityContext::current();
    let sudo_user = std::env::var("SUDO_USER").ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let candidates = candidate_search_dirs_with(sudo_user.as_deref(), home.as_deref());
    resolve_firecracker_binaries_with(env_bin_dir, &ctx, &candidates)
}

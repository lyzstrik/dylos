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

/// Account information retrieved from the system user database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub dir: PathBuf,
}

impl PasswdUser {
    /// Looks up a user account by name from the system user database.
    #[must_use]
    pub fn from_system(name: &str) -> Option<Self> {
        nix::unistd::User::from_name(name)
            .ok()
            .flatten()
            .map(|u| Self {
                name: u.name,
                uid: u.uid.as_raw(),
                gid: u.gid.as_raw(),
                dir: u.dir,
            })
    }
}

/// Resolves and validates invoking user identity under `sudo`.
///
/// Under `sudo`, the orchestrator process runs as root (`process_uid == 0`). Non-root ownership
/// and per-user directories are only trusted when:
/// 1. The current process is root (`process_uid == 0`).
/// 2. `SUDO_USER` is set, non-empty, and resolves to an account in the user database.
/// 3. `SUDO_UID` is set, parseable, and matches the user database UID.
/// 4. If `SUDO_GID` is set, it parses and matches the user database primary GID.
///
/// If any condition fails, the sudo environment is untrusted/inconsistent and fails closed
/// by returning `None` (skipping non-root user locations and falling back to direct-root behavior).
#[must_use]
#[allow(clippy::similar_names)]
pub fn resolve_sudo_identity_with<F>(
    process_uid: u32,
    sudo_user: Option<&str>,
    sudo_uid: Option<&str>,
    sudo_gid: Option<&str>,
    lookup_user: F,
) -> Option<PasswdUser>
where
    F: Fn(&str) -> Option<PasswdUser>,
{
    if process_uid != 0 {
        return None;
    }
    let user_name = sudo_user.filter(|s| !s.is_empty())?;
    let parsed_uid = sudo_uid.and_then(|u| u.parse::<u32>().ok())?;
    let user = lookup_user(user_name)?;
    if user.uid != parsed_uid {
        return None;
    }
    if let Some(gid_str) = sudo_gid.filter(|s| !s.is_empty()) {
        let parsed_gid = gid_str.parse::<u32>().ok()?;
        if user.gid != parsed_gid {
            return None;
        }
    }
    Some(user)
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
        let sudo_user = std::env::var("SUDO_USER").ok();
        let sudo_uid = std::env::var("SUDO_UID").ok();
        let sudo_gid = std::env::var("SUDO_GID").ok();
        let sudo = resolve_sudo_identity_with(
            current_uid,
            sudo_user.as_deref(),
            sudo_uid.as_deref(),
            sudo_gid.as_deref(),
            PasswdUser::from_system,
        );
        Self::from_resolved(current_uid, current_gid, sudo.as_ref())
    }

    /// Constructs a security context from resolved identity components.
    #[must_use]
    #[allow(clippy::similar_names)]
    pub fn from_resolved(
        current_uid: u32,
        current_gid: u32,
        sudo_identity: Option<&PasswdUser>,
    ) -> Self {
        if current_uid == 0 {
            if let Some(user) = sudo_identity {
                Self {
                    orchestrator_uid: current_uid,
                    invoking_uid: user.uid,
                    invoking_gid: user.gid,
                }
            } else {
                Self {
                    orchestrator_uid: 0,
                    invoking_uid: 0,
                    invoking_gid: current_gid,
                }
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

/// Checks that a directory is not writable or owned by untrusted users.
///
/// # Errors
/// Returns an error if the directory metadata cannot be read, if the directory is owned by an
/// unprivileged user other than the invoking owner, if it is world-writable, or if it is
/// group-writable by any non-root group.
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

    if (meta.mode() & 0o020 != 0) && meta.gid() != 0 {
        bail!(
            "parent directory '{}' of Firecracker binary is group-writable by non-root gid {} (mode {:o}); refusing to execute",
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
/// an unprivileged user other than the invoking owner, is world-writable, or is group-writable
/// by any non-root group.
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

    if (meta.mode() & 0o020 != 0) && meta.gid() != 0 {
        bail!(
            "Firecracker binary '{}' is group-writable by non-root gid {} (mode {:o}); refusing to execute",
            path.display(),
            meta.gid(),
            meta.mode() & 0o7777
        );
    }

    Ok(())
}

/// Validates binary path and all its ancestor directories against the security context.
///
/// Returns the canonical path to the executable. Note on execution safety: while validation checks
/// the path before runtime reopens or executes it, time-of-check to time-of-use (TOCTOU) replacement
/// is mitigated by strict ownership and permission enforcement: every ancestor directory and the binary
/// itself must be owned exclusively by root (uid 0) or the validated invoking user, and cannot be
/// world-writable or group-writable (unless group 0). Consequently, unprivileged users have no authority
/// to modify, replace, or rename any component along the canonical path.
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
///
/// Order of precedence:
/// 1. `~<SUDO_USER>/.local/share/dylos/bin` (when executed under `sudo` with verified identity)
/// 2. `$HOME/.local/share/dylos/bin` (the process's own home directory, if distinct)
/// 3. `/usr/local/lib/dylos/bin` (system-wide standard location)
/// 4. `/opt/dylos/bin` (system-wide vendor fallback)
#[must_use]
pub fn candidate_search_dirs_with(
    sudo_user_home: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user_home) = sudo_user_home {
        let user_bin = user_home.join(".local/share/dylos/bin");
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
#[allow(clippy::similar_names)]
pub fn resolve_firecracker_binaries(env_bin_dir: Option<&Path>) -> Result<ResolvedBinaries> {
    let current_uid = nix::unistd::geteuid().as_raw();
    let current_gid = nix::unistd::getegid().as_raw();
    let sudo_user = std::env::var("SUDO_USER").ok();
    let sudo_uid = std::env::var("SUDO_UID").ok();
    let sudo_gid = std::env::var("SUDO_GID").ok();
    let sudo_identity = resolve_sudo_identity_with(
        current_uid,
        sudo_user.as_deref(),
        sudo_uid.as_deref(),
        sudo_gid.as_deref(),
        PasswdUser::from_system,
    );
    let ctx = SecurityContext::from_resolved(current_uid, current_gid, sudo_identity.as_ref());
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let candidates = candidate_search_dirs_with(
        sudo_identity.as_ref().map(|u| u.dir.as_path()),
        home.as_deref(),
    );
    resolve_firecracker_binaries_with(env_bin_dir, &ctx, &candidates)
}

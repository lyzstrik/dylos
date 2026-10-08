use std::ffi::OsString;
use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Component;

use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};
use nix::unistd::{Gid, Uid, fchown, linkat};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Paths as Firecracker sees them, inside its chroot. They are the same for every VM and every
/// clone, so they can be stored in a snapshot.
pub const API_SOCKET: &str = "/run/firecracker.socket";
pub const METRICS_FILE: &str = "/run/metrics.json";

/// Jailer v1.17.0 creates and joins `<parent>/<id>` only when at least one `--cgroup` property
/// is given; without one it merely joins the parent. `pids.max=max` is the kernel default, so it
/// requests the per-VM cgroup without limiting anything.
const DEFAULT_CGROUP: &str = "pids.max=max";

/// How to start the jailer. Shared by every VM of a host.
#[derive(Debug, Clone)]
pub struct JailerConfig {
    pub jailer: PathBuf,
    pub firecracker: PathBuf,
    pub chroot_base_dir: PathBuf,
    /// The jailer drops to this uid/gid before exec'ing Firecracker; files placed in the chroot
    /// are chowned to it.
    pub uid: u32,
    pub gid: u32,
    /// Passed as `--parent-cgroup`; the jailer defaults it to the Firecracker file name.
    pub parent_cgroup: Option<String>,
    /// `<cgroup_file>=<value>` entries, each passed as `--cgroup` (cgroup v2). When empty,
    /// `pids.max=max` is passed so that the VM still gets its own cgroup.
    pub cgroups: Vec<String>,
    /// Mount point of the cgroup v2 hierarchy. The jailer finds it in `/proc/mounts`; this must
    /// match, since the VM cgroup is removed from here.
    pub cgroup_root: PathBuf,
    /// Arguments inserted before the jailer flags. Empty for the real jailer; lets tests run a
    /// fake launcher that needs its own leading arguments.
    pub launcher_args: Vec<OsString>,
    #[cfg(feature = "test-hooks")]
    /// Two-phase preparation barrier for integration tests.
    pub preparation_barrier: Option<std::sync::Arc<std::sync::Barrier>>,
    #[cfg(feature = "test-hooks")]
    /// Pauses after path validation and again after opening the source.
    pub source_validation_barrier: Option<std::sync::Arc<std::sync::Barrier>>,
    #[cfg(feature = "test-hooks")]
    pub cleanup_barrier: Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
}

impl JailerConfig {
    #[must_use]
    pub fn new(chroot_base_dir: impl Into<PathBuf>, uid: u32, gid: u32) -> Self {
        Self {
            jailer: PathBuf::from("/usr/bin/jailer"),
            firecracker: PathBuf::from("/usr/bin/firecracker"),
            chroot_base_dir: chroot_base_dir.into(),
            uid,
            gid,
            parent_cgroup: None,
            cgroups: Vec::new(),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            launcher_args: Vec::new(),
            #[cfg(feature = "test-hooks")]
            preparation_barrier: None,
            #[cfg(feature = "test-hooks")]
            source_validation_barrier: None,
            #[cfg(feature = "test-hooks")]
            cleanup_barrier: None,
        }
    }
}

/// A host file made available to Firecracker at `/<name>` inside the chroot.
///
/// It is hard-linked when possible, so the chroot shares the inode with `source`: callers pass
/// per-VM files (disks cloned by `dylos-store`), never a disk shared by several VMs.
#[derive(Debug, Clone)]
pub struct ChrootFile {
    pub source: PathBuf,
    pub name: String,
}

impl ChrootFile {
    pub fn new(source: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            name: name.into(),
        }
    }

    /// The path to give Firecracker, relative to the jail root.
    #[must_use]
    pub fn jailed_path(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Host-side locations of one VM's jail, all derived from the config and the jail id.
#[derive(Debug, Clone)]
pub struct JailPaths {
    pub id: String,
    /// `<chroot_base_dir>/<firecracker file name>/<id>`, the directory removed on cleanup.
    pub jail_dir: PathBuf,
    /// `<jail_dir>/root`, the directory Firecracker is chrooted into.
    pub root: PathBuf,
    pub cgroup_dir: PathBuf,
}

impl JailPaths {
    /// The jail id is `<lab_id>-<node>`, so it is stable across restarts and identical in every
    /// clone of a lab. The jailer only accepts 1 to 64 of `[A-Za-z0-9-]`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidJailId`] if the derived id is not accepted by the jailer.
    pub fn new(config: &JailerConfig, lab_id: &str, node: &str) -> Result<Self> {
        let id = format!("{lab_id}-{node}");
        if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(Error::InvalidJailId { id });
        }
        let exec_name = config
            .firecracker
            .file_name()
            .unwrap_or_else(|| "firecracker".as_ref());
        let jail_dir = config.chroot_base_dir.join(exec_name).join(&id);
        let parent_cgroup = config
            .parent_cgroup
            .as_deref()
            .map_or(exec_name, AsRef::as_ref);
        Ok(Self {
            root: jail_dir.join("root"),
            cgroup_dir: config.cgroup_root.join(parent_cgroup).join(&id),
            jail_dir,
            id,
        })
    }

    #[must_use]
    pub fn host_path(&self, jailed: &str) -> PathBuf {
        self.root.join(jailed.trim_start_matches('/'))
    }

    #[must_use]
    pub fn api_socket(&self) -> PathBuf {
        self.host_path(API_SOCKET)
    }
}

/// The full launcher command line, without the program itself.
#[must_use]
pub fn jailer_args(
    config: &JailerConfig,
    paths: &JailPaths,
    netns: Option<&Path>,
) -> Vec<OsString> {
    let mut flags: Vec<(&str, OsString)> = vec![
        ("--id", paths.id.clone().into()),
        ("--exec-file", config.firecracker.clone().into()),
        ("--uid", config.uid.to_string().into()),
        ("--gid", config.gid.to_string().into()),
        ("--chroot-base-dir", config.chroot_base_dir.clone().into()),
        ("--cgroup-version", "2".into()),
    ];
    if let Some(parent) = &config.parent_cgroup {
        flags.push(("--parent-cgroup", parent.into()));
    }
    if config.cgroups.is_empty() {
        flags.push(("--cgroup", DEFAULT_CGROUP.into()));
    }
    flags.extend(config.cgroups.iter().map(|c| ("--cgroup", c.into())));
    if let Some(netns) = netns {
        flags.push(("--netns", netns.into()));
    }
    let firecracker_flags = [("--api-sock", API_SOCKET), ("--metrics-path", METRICS_FILE)];
    let mut args = config.launcher_args.clone();
    for (flag, value) in flags {
        args.extend([flag.into(), value]);
    }
    args.push("--".into());
    for (flag, value) in firecracker_flags {
        args.extend([flag.into(), value.into()]);
    }
    args
}

/// Exclusive ownership of one VM's jail directory, which acts as the lock for its id: whoever
/// created it owns the jail and, once it has spawned the jailer, the VM cgroup.
///
/// [`Jail::release`] removes both. Dropping an unreleased `Jail` removes them synchronously; that
/// blocks, so it is only the fallback for errors and cancelled futures.
#[derive(Debug)]
#[must_use = "dropping a Jail removes it"]
pub struct Jail {
    paths: JailPaths,
    owned: bool,
    directory: File,
    #[cfg(feature = "test-hooks")]
    cleanup_barrier: Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
}

impl Jail {
    #[must_use]
    pub fn paths(&self) -> &JailPaths {
        &self.paths
    }

    /// Removes the cgroup, then the jail directory. On error the jail stays owned, so a later
    /// call or the drop can retry. Once released it is a no-op: the same paths may already
    /// belong to a new jail for the same VM.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on any failure other than "not found".
    pub async fn release(&mut self) -> Result<()> {
        if !self.owned {
            return Ok(());
        }
        #[cfg(feature = "test-hooks")]
        if let Some(barrier) = self.cleanup_barrier.take() {
            barrier.0.notify_one();
            barrier.1.notified().await;
        }
        remove_jail(&self.paths).await?;
        self.owned = false;
        Ok(())
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        let res = ignore_missing(std::fs::remove_dir(&self.paths.cgroup_dir))
            .and_then(|()| ignore_missing(std::fs::remove_dir_all(&self.paths.jail_dir)));
        if let Err(e) = res {
            tracing::error!(vm = %self.paths.id, error = %e, "failed to remove the jail");
        }
    }
}

/// Atomically claims the jail directory, then places `files`, the `/run` directory and the
/// metrics file in it, owned by the jailer uid/gid.
///
/// Everything runs in one blocking task that owns the [`Jail`]: if this future is dropped, the
/// task still finishes and the jail is removed when its unread result is dropped. On error, only
/// a directory this call created is removed.
///
/// # Errors
///
/// [`Error::JailExists`] if the jail directory is already there, [`Error::InvalidChrootFileName`],
/// [`Error::Io`], or [`Error::Task`].
pub async fn prepare_chroot(
    config: &JailerConfig,
    paths: &JailPaths,
    files: &[ChrootFile],
) -> Result<Jail> {
    for file in files {
        let name = file.name.as_str();
        if name.is_empty() || name == "." || name == ".." || name == "run" || name.contains('/') {
            return Err(Error::InvalidChrootFileName {
                id: paths.id.clone(),
                name: file.name.clone(),
            });
        }
    }
    let (paths, files, uid, gid) = (paths.clone(), files.to_vec(), config.uid, config.gid);
    let id = paths.id.clone();
    #[cfg(feature = "test-hooks")]
    let cleanup_barrier = config.cleanup_barrier.clone();
    #[cfg(feature = "test-hooks")]
    let barrier = config.preparation_barrier.clone();
    #[cfg(feature = "test-hooks")]
    let source_barrier = config.source_validation_barrier.clone();
    tokio::task::spawn_blocking(move || {
        let jail = claim(paths)?;
        #[cfg(feature = "test-hooks")]
        let jail = {
            let mut jail = jail;
            jail.cleanup_barrier = cleanup_barrier;
            jail
        };
        #[cfg(feature = "test-hooks")]
        if let Some(b) = barrier {
            b.wait();
            b.wait();
        }
        populate(
            &jail,
            &files,
            uid,
            gid,
            #[cfg(feature = "test-hooks")]
            source_barrier.as_deref(),
        )?;
        Ok(jail)
    })
    .await
    .map_err(|source| Error::Task {
        id,
        task: "chroot preparation",
        source,
    })?
}

fn io_error(
    paths: &JailPaths,
    op: &'static str,
    path: &Path,
) -> impl FnOnce(std::io::Error) -> Error + use<> {
    let (id, path) = (paths.id.clone(), path.to_path_buf());
    move |source| Error::Io {
        id,
        op,
        path,
        source,
    }
}

fn claim(paths: JailPaths) -> Result<Jail> {
    let jail_dir = &paths.jail_dir;
    let parent = jail_dir.parent().ok_or_else(|| {
        io_error(&paths, "open parent", jail_dir)(std::io::Error::from(ErrorKind::InvalidInput))
    })?;
    let directory =
        open_directory(parent, true).map_err(io_error(&paths, "open parent", parent))?;
    match mkdirat(
        &directory,
        paths.id.as_str(),
        Mode::from_bits_truncate(0o700),
    ) {
        Ok(()) => {
            let mut jail = Jail {
                paths,
                directory,
                owned: true,
                #[cfg(feature = "test-hooks")]
                cleanup_barrier: None,
            };
            jail.directory = openat(
                &jail.directory,
                jail.paths.id.as_str(),
                directory_flags(),
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|e| io_error(&jail.paths, "open jail", &jail.paths.jail_dir)(e.into()))?;
            Ok(jail)
        }
        Err(nix::errno::Errno::EEXIST) => Err(Error::JailExists {
            path: jail_dir.clone(),
            id: paths.id,
        }),
        Err(e) => Err(io_error(&paths, "create dir", jail_dir)(e.into())),
    }
}

fn directory_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

// Resolve each component relative to a pinned directory, so replacing any ancestor with a
// symlink cannot redirect a later open, mkdir, link or ownership change.
fn open_directory(path: &Path, create: bool) -> std::io::Result<File> {
    let mut dir = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        let name = match component {
            Component::Normal(name) => name,
            Component::ParentDir => "..".as_ref(),
            Component::RootDir | Component::CurDir => continue,
            Component::Prefix(_) => return Err(ErrorKind::InvalidInput.into()),
        };
        if create {
            match mkdirat(&dir, name, Mode::from_bits_truncate(0o755)) {
                Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        dir = File::from(openat(&dir, name, directory_flags(), Mode::empty())?);
    }
    Ok(dir)
}

fn populate(
    jail: &Jail,
    files: &[ChrootFile],
    uid: u32,
    gid: u32,
    #[cfg(feature = "test-hooks")] source_barrier: Option<&std::sync::Barrier>,
) -> Result<()> {
    let paths = &jail.paths;
    let io = |op, path: &Path| io_error(paths, op, path);
    let own = |file: &File, path: &Path| {
        fchown(file, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
            .map_err(|e| io("fchown", path)(e.into()))
    };
    let make_dir = |parent: &File, name: &str, path: &Path| -> Result<File> {
        mkdirat(parent, name, Mode::from_bits_truncate(0o755))
            .map_err(|e| io("create dir", path)(e.into()))?;
        openat(parent, name, directory_flags(), Mode::empty())
            .map(File::from)
            .map_err(|e| io("open dir", path)(e.into()))
    };
    let root = make_dir(&jail.directory, "root", &paths.root)?;
    let run_path = paths.host_path("/run");
    let run = make_dir(&root, "run", &run_path)?;
    for file in files {
        let placed = place_file(
            paths,
            &root,
            file,
            #[cfg(feature = "test-hooks")]
            source_barrier,
        )?;
        own(&placed, &paths.host_path(&file.jailed_path()))?;
    }
    let metrics_path = paths.host_path(METRICS_FILE);
    let metrics = openat(
        &run,
        "metrics.json",
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .map(File::from)
    .map_err(|e| io("create metrics", &metrics_path)(e.into()))?;
    own(&metrics, &metrics_path)?;
    own(&run, &run_path)?;
    own(&root, &paths.root)?;
    Ok(())
}

fn place_file(
    paths: &JailPaths,
    root: &File,
    file: &ChrootFile,
    #[cfg(feature = "test-hooks")] source_barrier: Option<&std::sync::Barrier>,
) -> Result<File> {
    let io = |op, path: &Path| io_error(paths, op, path);
    let metadata =
        std::fs::symlink_metadata(&file.source).map_err(io("validate source", &file.source))?;
    if !metadata.is_file() {
        return Err(io("validate regular source", &file.source)(
            ErrorKind::InvalidInput.into(),
        ));
    }
    #[cfg(feature = "test-hooks")]
    if let Some(barrier) = source_barrier {
        barrier.wait();
        barrier.wait();
    }
    let parent = file
        .source
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = file
        .source
        .file_name()
        .ok_or_else(|| io("open source", &file.source)(ErrorKind::InvalidInput.into()))?;
    let source_dir = open_directory(parent, false).map_err(io("open source parent", parent))?;
    // NONBLOCK prevents a regular file replaced by a FIFO from hanging preparation.
    let mut source = openat(
        &source_dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|e| io("open source", &file.source)(e.into()))?;
    let pinned = source.metadata().map_err(io("stat source", &file.source))?;
    if !pinned.is_file() || (metadata.dev(), metadata.ino()) != (pinned.dev(), pinned.ino()) {
        return Err(io("source changed", &file.source)(
            ErrorKind::InvalidInput.into(),
        ));
    }
    #[cfg(feature = "test-hooks")]
    if let Some(barrier) = source_barrier {
        barrier.wait();
        barrier.wait();
    }
    let dest = paths.host_path(&file.jailed_path());
    let placed = match linkat(
        &source_dir,
        name,
        root,
        file.name.as_str(),
        AtFlags::empty(),
    ) {
        Err(nix::errno::Errno::EXDEV) => {
            let mut placed = openat(
                root,
                file.name.as_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(pinned.mode() & 0o777),
            )
            .map(File::from)
            .map_err(|e| io("create copy", &dest)(e.into()))?;
            std::io::copy(&mut source, &mut placed).map_err(io("copy", &file.source))?;
            placed
        }
        result => {
            result.map_err(|e| io("hard link", &file.source)(e.into()))?;
            let placed = openat(
                root,
                file.name.as_str(),
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|e| io("open placed file", &dest)(e.into()))?;
            let linked = placed.metadata().map_err(io("stat placed file", &dest))?;
            // linkat does not follow symlinks; verify it linked the inode we validated,
            // rather than a replacement inserted after opening the source.
            if !linked.is_file() || (linked.dev(), linked.ino()) != (pinned.dev(), pinned.ino()) {
                return Err(io("source changed during placement", &file.source)(
                    ErrorKind::InvalidInput.into(),
                ));
            }
            placed
        }
    };
    Ok(placed)
}

fn ignore_missing(res: std::io::Result<()>) -> std::io::Result<()> {
    match res {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        res => res,
    }
}

/// Removes the VM cgroup, then the jail directory, so the directory, which is the lock for the
/// id, goes last. Succeeds if they are already gone. The cgroup can only be removed once the
/// process has been reaped.
///
/// # Errors
///
/// [`Error::Io`] on any failure other than "not found".
pub async fn remove_jail(paths: &JailPaths) -> Result<()> {
    let res = tokio::fs::remove_dir(&paths.cgroup_dir).await;
    ignore_missing(res).map_err(io_error(paths, "remove cgroup", &paths.cgroup_dir))?;
    let res = tokio::fs::remove_dir_all(&paths.jail_dir).await;
    ignore_missing(res).map_err(io_error(paths, "remove dir", &paths.jail_dir))
}

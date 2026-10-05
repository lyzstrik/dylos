use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Paths as Firecracker sees them, inside its chroot. They are the same for every VM and every
/// clone, so they can be stored in a snapshot.
pub const API_SOCKET: &str = "/run/firecracker.socket";
pub const METRICS_FILE: &str = "/run/metrics.json";

const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";

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
    /// `<cgroup_file>=<value>` entries, each passed as `--cgroup` (cgroup v2).
    pub cgroups: Vec<String>,
    /// Arguments inserted before the jailer flags. Empty for the real jailer; lets tests run a
    /// fake launcher that needs its own leading arguments.
    pub launcher_args: Vec<OsString>,
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
            launcher_args: Vec::new(),
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
            cgroup_dir: Path::new(CGROUP_V2_ROOT).join(parent_cgroup).join(&id),
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

/// Creates the jail directory and places `files`, the `/run` directory and the metrics file in
/// it, owned by the jailer uid/gid. Rolls back on error.
///
/// # Errors
///
/// [`Error::JailExists`] if the jail directory is already there, [`Error::InvalidChrootFileName`],
/// or [`Error::Io`].
pub async fn prepare_chroot(
    config: &JailerConfig,
    paths: &JailPaths,
    files: &[ChrootFile],
) -> Result<()> {
    for file in files {
        let name = file.name.as_str();
        if name.is_empty() || name == "." || name == ".." || name == "run" || name.contains('/') {
            return Err(Error::InvalidChrootFileName {
                id: paths.id.clone(),
                name: file.name.clone(),
            });
        }
    }
    if tokio::fs::try_exists(&paths.jail_dir).await.unwrap_or(true) {
        return Err(Error::JailExists {
            id: paths.id.clone(),
            path: paths.jail_dir.clone(),
        });
    }
    let res = populate(config, paths, files).await;
    if res.is_err()
        && let Err(e) = remove_jail(paths).await
    {
        tracing::warn!(vm = %paths.id, error = %e, "rollback of the jail directory failed");
    }
    res
}

async fn populate(config: &JailerConfig, paths: &JailPaths, files: &[ChrootFile]) -> Result<()> {
    let io = |op, path: &Path| {
        let (id, path) = (paths.id.clone(), path.to_path_buf());
        move |source| Error::Io {
            id,
            op,
            path,
            source,
        }
    };
    let run = paths.host_path("/run");
    tokio::fs::create_dir_all(&run)
        .await
        .map_err(io("create dir", &run))?;
    let mut owned = vec![run];
    for file in files {
        let dest = paths.host_path(&file.jailed_path());
        match tokio::fs::hard_link(&file.source, &dest).await {
            Err(e) if e.kind() == ErrorKind::CrossesDevices => {
                tokio::fs::copy(&file.source, &dest)
                    .await
                    .map(drop)
                    .map_err(io("copy", &file.source))?;
            }
            res => res.map_err(io("hard link", &file.source))?,
        }
        owned.push(dest);
    }
    let metrics = paths.host_path(METRICS_FILE);
    tokio::fs::File::create(&metrics)
        .await
        .map_err(io("create", &metrics))?;
    owned.push(metrics);
    let (uid, gid) = (config.uid, config.gid);
    for path in owned {
        let p = path.clone();
        tokio::task::spawn_blocking(move || std::os::unix::fs::chown(&p, Some(uid), Some(gid)))
            .await
            .map_err(std::io::Error::other)
            .flatten()
            .map_err(io("chown", &path))?;
    }
    Ok(())
}

/// Removes the jail directory and the VM cgroup. Succeeds if they are already gone. The cgroup
/// can only be removed once the process has been reaped.
///
/// # Errors
///
/// [`Error::Io`] on any failure other than "not found".
pub async fn remove_jail(paths: &JailPaths) -> Result<()> {
    let ignore_missing = |res: std::io::Result<()>, op, path: &Path| match res {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(Error::Io {
            id: paths.id.clone(),
            op,
            path: path.to_path_buf(),
            source: e,
        }),
        _ => Ok(()),
    };
    let res = tokio::fs::remove_dir_all(&paths.jail_dir).await;
    ignore_missing(res, "remove dir", &paths.jail_dir)?;
    let res = tokio::fs::remove_dir(&paths.cgroup_dir).await;
    ignore_missing(res, "remove cgroup", &paths.cgroup_dir)
}

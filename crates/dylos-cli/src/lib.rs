//! Library interface for the `dylos` CLI.
#![forbid(unsafe_code)]

pub mod binaries;

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dylos_core::LabSpec;
use dylos_runtime::{LabConfig, jailer::JailPaths};
use nix::fcntl::{Flock, FlockArg, OFlag, openat};
use nix::sys::stat::Mode;

/// A point-in-time observation, which can race with lifecycle operations.
#[derive(Debug)]
pub struct LabListing {
    pub id: String,
    pub state: &'static str,
    pub vms: usize,
    pub directory: PathBuf,
}

fn open_directory(path: &Path) -> Result<File> {
    let flags = nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_DIRECTORY;
    OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// Inspect labs without creating files, connecting sockets, or changing resources.
/// `proc_dir` selects the procfs mount to inspect.
///
/// # Errors
/// Reports unreadable or malformed state rather than reporting a false stopped state.
pub fn list_labs(config: &LabConfig, proc_dir: &Path) -> Result<Vec<LabListing>> {
    let entries = match fs::read_dir(&config.store.base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("read lab store"),
    };
    let mut labs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir()
            || (id == "snapshots" && !entry.path().join("state.json").try_exists()?)
        {
            continue;
        }
        dylos_core::validate_name(&id, 32).map_err(anyhow::Error::msg)?;
        let directory = entry.path();
        let dir = open_directory(&directory)?;
        let pinned = dir.try_clone()?;
        // The runtime owns this exact directory lock for the supervisor's lifetime.
        let supervised = match Flock::lock(dir, FlockArg::LockSharedNonblock) {
            Ok(lock) => {
                drop(lock);
                false
            }
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => true,
            Err((_, error)) => return Err(error).context("probe supervisor lock"),
        };
        let state_path = directory.join("state.json");
        let file = File::from(
            openat(
                &pinned,
                "state.json",
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| format!("open {}", state_path.display()))?,
        );
        if !file.metadata()?.is_file() {
            bail!("{} is not a regular file", state_path.display());
        }
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            bail!("{} exceeds 1 MiB", state_path.display());
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", state_path.display()))?;
        if value["lab_id"].as_str() != Some(&id) {
            bail!("lab id mismatch in {}", state_path.display());
        }
        let spec: LabSpec = serde_json::from_value(value["spec"].clone())?;
        spec.validate()?;
        let namespace = match fs::symlink_metadata(config.netns_dir.join(format!("dylos-{id}"))) {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::NotFound => false,
            Err(e) => return Err(e).context("inspect lab namespace"),
        };
        let mut owned_groups = Vec::new();
        for node in &spec.nodes {
            let paths =
                JailPaths::new(&config.jailer, &format!("lab{}-{id}", id.len()), &node.name)?;
            owned_groups.push(
                paths
                    .cgroup_dir
                    .strip_prefix(&config.jailer.cgroup_root)?
                    .to_path_buf(),
            );
        }
        let live = live_vms(proc_dir, &owned_groups)?;
        let state = if supervised && namespace && live == owned_groups.len() {
            "running"
        } else if !supervised && !namespace && live == 0 {
            "stopped"
        } else {
            "orphaned"
        };
        labs.push(LabListing {
            id,
            state,
            vms: spec.nodes.len(),
            directory,
        });
    }
    labs.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(labs)
}

fn live_vms(proc_dir: &Path, owned_groups: &[PathBuf]) -> Result<usize> {
    let mut live = std::collections::HashSet::new();
    for process in fs::read_dir(proc_dir).context("read procfs")? {
        let process = process?;
        if process
            .file_name()
            .to_str()
            .is_none_or(|s| s.parse::<u32>().is_err())
        {
            continue;
        }
        // Cgroup membership identifies owned VMs without trusting saved PIDs or
        // needing permission to dereference another user's /proc/<pid>/root.
        let groups = match fs::read_to_string(process.path().join("cgroup")) {
            Ok(groups) => groups,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("read process cgroup membership"),
        };
        for (index, group) in owned_groups.iter().enumerate() {
            if groups
                .lines()
                .filter_map(|line| line.splitn(3, ':').nth(2))
                .any(|path| {
                    Path::new(path)
                        .strip_prefix("/")
                        .is_ok_and(|p| p == group.as_path())
                })
            {
                live.insert(index);
            }
        }
    }
    Ok(live.len())
}

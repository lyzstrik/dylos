//! Synchronous, descriptor-relative lab storage. Async callers must use `spawn_blocking`.
use dylos_core::LabSpec;
use nix::{
    dir::Dir,
    errno::Errno,
    fcntl::{OFlag, openat},
    sys::stat::{Mode, mkdirat},
    unistd::{UnlinkatFlags, unlinkat},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Component, Path, PathBuf},
    time::{Instant, SystemTime},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid lab id: {0}")]
    InvalidId(String),
    #[error(transparent)]
    Spec(#[from] dylos_core::Error),
    #[error("storage operation on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "reflink required for {path}; check that source and destination share a reflink filesystem: {source}"
    )]
    Reflink {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("serialize lab state: {0}")]
    State(#[from] serde_json::Error),
}
fn contextual(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
fn valid_id(id: &str) -> Result<(), Error> {
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Error::InvalidId(id.into()));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Store {
    pub base: PathBuf,
    pub image: PathBuf,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            base: "target/labs".into(),
            image: "target/images/rootfs.ext4".into(),
        }
    }
}
#[derive(Debug, Clone)]
pub struct VmPaths {
    pub directory: PathBuf,
    pub rootfs: PathBuf,
    pub control_socket: PathBuf,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub lab_id: String,
    pub spec: LabSpec,
    pub created_at: SystemTime,
}

/// Exclusive ownership of a lab; dropping removes it. Call `remove` to observe cleanup errors.
#[derive(Debug)]
#[must_use]
pub struct Lab {
    root: PathBuf,
    parent: File,
    directory: File,
    id: String,
    owned: bool,
}
impl Lab {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }
    /// # Errors
    /// Rejects names that could escape the lab directory.
    pub fn vm_paths(&self, node: &str) -> Result<VmPaths, Error> {
        valid_id(node)?;
        let directory = self.root.join("vms").join(node);
        Ok(VmPaths {
            rootfs: directory.join("rootfs.ext4"),
            control_socket: directory.join("control.sock"),
            directory,
        })
    }
    /// # Errors
    /// Reports filesystem errors and refuses a replaced lab directory.
    pub fn remove(&mut self) -> Result<(), Error> {
        if self.owned {
            remove_owned(&self.parent, &self.directory, &self.id)
                .map_err(|e| contextual(&self.root, e))?;
            self.owned = false;
        }
        Ok(())
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::error!(lab = %self.id, %error, "lab cleanup failed");
        }
    }
}
impl Store {
    /// # Errors
    /// Rejects invalid specs, existing labs, symlinks, and unsupported reflinks. Rolls back on failure.
    pub fn create(&self, id: &str, spec: &LabSpec) -> Result<Lab, Error> {
        valid_id(id)?;
        spec.validate()?;
        let _span = tracing::info_span!("store_create", lab = id).entered();
        let started = Instant::now();
        let parent = directory(&self.base, true).map_err(|e| contextual(&self.base, e))?;
        let root = self.base.join(id);
        mkdirat(&parent, id, Mode::from_bits_truncate(0o700))
            .map_err(|e| contextual(&root, e.into()))?;
        let pinned = match open(&parent, id.as_ref(), dirs()) {
            Ok(file) => file,
            Err(error) => {
                unlinkat(&parent, id, UnlinkatFlags::RemoveDir)
                    .map_err(|e| contextual(&root, e.into()))?;
                return Err(contextual(&root, error));
            }
        };
        let lab = Lab {
            root,
            parent,
            directory: pinned,
            id: id.into(),
            owned: true,
        };
        let result = (|| {
            let source_parent = self
                .image
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let source_dir =
                directory(source_parent, false).map_err(|e| contextual(&self.image, e))?;
            let name = self
                .image
                .file_name()
                .ok_or_else(|| contextual(&self.image, io::ErrorKind::InvalidInput.into()))?;
            let source = open(&source_dir, name, OFlag::O_RDONLY | OFlag::O_NONBLOCK)
                .map_err(|e| contextual(&self.image, e))?;
            if !source
                .metadata()
                .map_err(|e| contextual(&self.image, e))?
                .is_file()
            {
                return Err(contextual(&self.image, io::ErrorKind::InvalidInput.into()));
            }
            let vms =
                make_directory(&lab.directory, "vms").map_err(|e| contextual(&lab.root, e))?;
            for node in &spec.nodes {
                let _vm = tracing::info_span!("store_vm", vm = %node.name).entered();
                let vm_started = Instant::now();
                let paths = lab.vm_paths(&node.name)?;
                let vm = make_directory(&vms, &node.name)
                    .map_err(|e| contextual(&paths.directory, e))?;
                let destination = open(
                    &vm,
                    "rootfs.ext4".as_ref(),
                    OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
                )
                .map_err(|e| contextual(&paths.rootfs, e))?;
                reflink(&source, &destination).map_err(|source| Error::Reflink {
                    path: paths.rootfs,
                    source,
                })?;
                tracing::debug!(elapsed = ?vm_started.elapsed(), "rootfs reflink complete");
            }
            let state = State {
                lab_id: id.into(),
                spec: spec.clone(),
                created_at: SystemTime::now(),
            };
            let bytes = serde_json::to_vec(&state)?;
            let mut file = open(
                &lab.directory,
                "state.json".as_ref(),
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            )
            .map_err(|e| contextual(&lab.state_path(), e))?;
            file.write_all(&bytes)
                .map_err(|e| contextual(&lab.state_path(), e))?;
            Ok(())
        })();
        if let Err(error) = result {
            let mut lab = lab;
            lab.remove()?;
            return Err(error);
        }
        tracing::debug!(elapsed = ?started.elapsed(), "lab creation complete");
        Ok(lab)
    }
    /// Removes even a partially created lab. Symlinks are unlinked, never traversed.
    /// # Errors
    /// Reports filesystem errors or an invalid id.
    pub fn remove(&self, id: &str) -> Result<(), Error> {
        valid_id(id)?;
        let result = (|| {
            let parent = directory(&self.base, false)?;
            remove_entry(&parent, id.as_ref())
        })();
        match result {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other.map_err(|e| contextual(&self.base.join(id), e)),
        }
    }
}
fn dirs() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY
}
fn open(parent: &File, name: &std::ffi::OsStr, flags: OFlag) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?))
}
fn make_directory(parent: &File, name: &str) -> io::Result<File> {
    mkdirat(parent, name, Mode::from_bits_truncate(0o700))?;
    open(parent, name.as_ref(), dirs())
}
// Pin every ancestor; O_NOFOLLOW on only the final component would still allow escape.
fn directory(path: &Path, create: bool) -> io::Result<File> {
    let mut parent = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        let name = match component {
            Component::Normal(name) => name,
            Component::RootDir | Component::CurDir => continue,
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        };
        if create {
            match mkdirat(&parent, name, Mode::from_bits_truncate(0o700)) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        parent = open(&parent, name, dirs())?;
    }
    Ok(parent)
}
fn reflink(source: &File, destination: &File) -> io::Result<()> {
    // SAFETY: FICLONE takes a valid source descriptor as its integer argument, not a pointer;
    // both Files remain live for the ioctl and the destination is opened writable.
    let result = unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn remove_owned(parent: &File, directory: &File, id: &str) -> io::Result<()> {
    let current = match open(parent, id.as_ref(), dirs()) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    let (a, b) = (current.metadata()?, directory.metadata()?);
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    clear(directory)?;
    unlinkat(parent, id, UnlinkatFlags::RemoveDir)?;
    Ok(())
}
fn clear(directory: &File) -> io::Result<()> {
    let mut entries = Dir::from_fd(open(directory, ".".as_ref(), dirs())?.into())?;
    for entry in entries.iter() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        remove_entry(directory, std::os::unix::ffi::OsStrExt::from_bytes(name))?;
    }
    Ok(())
}
fn remove_entry(parent: &File, name: &std::ffi::OsStr) -> io::Result<()> {
    match open(parent, name, dirs()) {
        Ok(directory) => {
            clear(&directory)?;
            unlinkat(parent, name, UnlinkatFlags::RemoveDir)?;
        }
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
            unlinkat(parent, name, UnlinkatFlags::NoRemoveDir)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

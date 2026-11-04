//! Lab directory layout and reflink cloning of disks.
//!
//! Synchronous, descriptor-relative lab storage. Async callers must use `spawn_blocking`.
//!
//! # Current status of the spike
//!
//! `dylos-store` implements the lab directory layout and reflink rootfs copies (LYZ-50).
//! Lab bring-up (LYZ-51) and the CLI (LYZ-52) are still to come.
//!
//! # Target Design Constraints
//!
//! This crate manages the host directory structure for a lab and provides fast
//! copy-on-write disk cloning via `ioctl_ficlone`.
//! It depends on `dylos-core`, but must never depend on higher-level orchestration
//! crates (like `dylos-runtime` or `dylos-cli`).

use dylos_core::LabSpec;
use dylos_core::manifest::{FileMeta, SnapshotManifest};
use nix::{
    dir::Dir,
    errno::Errno,
    fcntl::{OFlag, openat, renameat},
    sys::stat::{Mode, mkdirat},
    unistd::{UnlinkatFlags, unlinkat},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
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

fn valid_file_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > 64
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(Error::InvalidId(name.into()));
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

#[derive(Debug, Clone)]
pub struct SnapshotVmPaths {
    pub directory: PathBuf,
    pub state_file: PathBuf,
    pub memory_file: PathBuf,
    pub rootfs: PathBuf,
}

/// Exclusive ownership of a snapshot directory during creation; dropping removes it unless committed.
#[derive(Debug)]
#[must_use]
pub struct Snapshot {
    root: PathBuf,
    parent: File,
    directory: File,
    lab_id: String,
    id: String,
    owned: bool,
}

/// Timings recorded during a VM disk clone operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskCloneTimings {
    pub fsync_ms: u64,
    pub reflink_ms: u64,
    pub hash_ms: u64,
}

/// A VM state or memory file, opened once without following symlinks. Saving it reads and
/// moves this descriptor only: a later replacement of the path is neither read nor unlinked.
#[derive(Debug)]
pub struct VmFileSource {
    path: PathBuf,
    dir: File,
    name: std::ffi::OsString,
    file: File,
    meta: std::fs::Metadata,
    max_bytes: u64,
}

impl VmFileSource {
    /// # Errors
    /// Rejects symlinks in any component, non-regular files and files over `max_bytes`.
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self, Error> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let dir = directory(parent, false).map_err(|e| contextual(path, e))?;
        let name = path
            .file_name()
            .ok_or_else(|| contextual(path, io::ErrorKind::InvalidInput.into()))?
            .to_owned();
        // O_NONBLOCK: opening a FIFO must fail the regular-file check, not hang.
        let file = open(&dir, &name, OFlag::O_RDONLY | OFlag::O_NONBLOCK)
            .map_err(|e| contextual(path, e))?;
        let meta = file.metadata().map_err(|e| contextual(path, e))?;
        if !meta.is_file() {
            return Err(contextual(
                path,
                io::Error::new(io::ErrorKind::InvalidInput, "source is not a regular file"),
            ));
        }
        if meta.len() > max_bytes {
            return Err(contextual(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "source file size {} exceeds maximum allowed bound of {max_bytes} bytes",
                        meta.len()
                    ),
                ),
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            dir,
            name,
            file,
            meta,
            max_bytes,
        })
    }
}

impl Snapshot {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn lab_id(&self) -> &str {
        &self.lab_id
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// # Errors
    /// Rejects names that could escape the snapshot directory.
    pub fn vm_paths(&self, node: &str) -> Result<SnapshotVmPaths, Error> {
        valid_id(node)?;
        let directory = self.root.join("vms").join(node);
        Ok(SnapshotVmPaths {
            state_file: directory.join("vm.state"),
            memory_file: directory.join("vm.mem"),
            rootfs: directory.join("rootfs.ext4"),
            directory,
        })
    }

    /// Clones a VM disk into the snapshot by reflink after an fsync of the source disk.
    ///
    /// # Errors
    /// Returns [`Error::Io`] or [`Error::Reflink`].
    pub fn clone_disk(
        &self,
        source_path: &Path,
        node: &str,
        disk_name: &str,
    ) -> Result<FileMeta, Error> {
        self.clone_disk_with_timings(source_path, node, disk_name)
            .map(|(meta, _)| meta)
    }

    /// Clones a VM disk into the snapshot, returning fine-grained step durations.
    ///
    /// # Errors
    /// Returns [`Error::Io`] or [`Error::Reflink`].
    pub fn clone_disk_with_timings(
        &self,
        source_path: &Path,
        node: &str,
        disk_name: &str,
    ) -> Result<(FileMeta, DiskCloneTimings), Error> {
        valid_id(node)?;
        valid_file_name(disk_name)?;
        let _span = tracing::info_span!("clone_disk", node = %node, disk = %disk_name).entered();
        let started = Instant::now();

        let source_parent = source_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let source_dir = directory(source_parent, false).map_err(|e| contextual(source_path, e))?;
        let name = source_path
            .file_name()
            .ok_or_else(|| contextual(source_path, io::ErrorKind::InvalidInput.into()))?;
        let source_file = open(&source_dir, name, OFlag::O_RDONLY | OFlag::O_NONBLOCK)
            .map_err(|e| contextual(source_path, e))?;
        if !source_file
            .metadata()
            .map_err(|e| contextual(source_path, e))?
            .is_file()
        {
            return Err(contextual(source_path, io::ErrorKind::InvalidInput.into()));
        }
        let t_fsync = Instant::now();
        source_file
            .sync_all()
            .map_err(|e| contextual(source_path, e))?;
        let fsync_ms = u64::try_from(t_fsync.elapsed().as_millis()).unwrap_or(u64::MAX);

        let vms = open(&self.directory, "vms".as_ref(), dirs())
            .map_err(|e| contextual(&self.root.join("vms"), e))?;
        let vm_dir = match open(&vms, node.as_ref(), dirs()) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => make_directory(&vms, node)
                .map_err(|e| contextual(&self.root.join("vms").join(node), e))?,
            Err(e) => return Err(contextual(&self.root.join("vms").join(node), e)),
        };

        let dest_path = self.root.join("vms").join(node).join(disk_name);
        let dest_file = open(
            &vm_dir,
            disk_name.as_ref(),
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL,
        )
        .map_err(|e| contextual(&dest_path, e))?;

        let t_reflink = Instant::now();
        reflink(&source_file, &dest_file).map_err(|source| Error::Reflink {
            path: dest_path.clone(),
            source,
        })?;
        let reflink_ms = u64::try_from(t_reflink.elapsed().as_millis()).unwrap_or(u64::MAX);

        let t_hash = Instant::now();
        let (sha256, size) = compute_file_sha256_and_size(&dest_file, u64::MAX)
            .map_err(|e| contextual(&dest_path, e))?;
        let hash_ms = u64::try_from(t_hash.elapsed().as_millis()).unwrap_or(u64::MAX);

        tracing::debug!(
            elapsed = ?started.elapsed(),
            size,
            fsync_ms,
            reflink_ms,
            hash_ms,
            "disk cloned by reflink"
        );
        Ok((
            FileMeta {
                path: PathBuf::from(format!("vms/{node}/{disk_name}")),
                sha256,
                size,
            },
            DiskCloneTimings {
                fsync_ms,
                reflink_ms,
                hash_ms,
            },
        ))
    }

    /// Moves or reflinks a VM state or memory file into the snapshot directory, measuring hash duration.
    ///
    /// # Errors
    /// Returns [`Error::Io`] or [`Error::Reflink`].
    pub fn save_vm_file_with_timings(
        &self,
        source_path: &Path,
        node: &str,
        file_name: &str,
        max_bytes: u64,
    ) -> Result<(FileMeta, u64), Error> {
        self.save_vm_source(VmFileSource::open(source_path, max_bytes)?, node, file_name)
    }

    /// Moves or reflinks a VM state or memory file into the snapshot directory.
    ///
    /// # Errors
    /// Returns [`Error::Io`] or [`Error::Reflink`].
    pub fn save_vm_file(
        &self,
        source_path: &Path,
        node: &str,
        file_name: &str,
        max_bytes: u64,
    ) -> Result<FileMeta, Error> {
        self.save_vm_file_with_timings(source_path, node, file_name, max_bytes)
            .map(|(meta, _)| meta)
    }

    /// Moves an opened VM file into the snapshot directory: reflink, or a bounded copy when the
    /// filesystem has no reflink, then the source is unlinked only after the bounded hash
    /// succeeded and only if its path still names the opened inode.
    ///
    /// # Errors
    /// Fails closed on a cross-device destination and on growth past the source bound; the
    /// partial destination is removed and the source kept.
    pub fn save_vm_source(
        &self,
        source: VmFileSource,
        node: &str,
        file_name: &str,
    ) -> Result<(FileMeta, u64), Error> {
        valid_id(node)?;
        valid_file_name(file_name)?;
        let VmFileSource {
            path: source_path,
            dir: source_dir,
            name: source_name,
            file: source_file,
            meta: source_meta,
            max_bytes,
        } = source;
        let source_path = source_path.as_path();

        let vms = open(&self.directory, "vms".as_ref(), dirs())
            .map_err(|e| contextual(&self.root.join("vms"), e))?;
        let vm_dir = match open(&vms, node.as_ref(), dirs()) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => make_directory(&vms, node)
                .map_err(|e| contextual(&self.root.join("vms").join(node), e))?,
            Err(e) => return Err(contextual(&self.root.join("vms").join(node), e)),
        };

        let dest_path = self.root.join("vms").join(node).join(file_name);
        let mut dest_file = open(
            &vm_dir,
            file_name.as_ref(),
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL,
        )
        .map_err(|e| contextual(&dest_path, e))?;
        let dest_meta = dest_file
            .metadata()
            .map_err(|e| contextual(&dest_path, e))?;
        let fail = |err: io::Error| with_cleanup(err, cleanup_dest(&vm_dir, file_name, &dest_path));

        // A cross-device source is never copied: snapshot files must stay on the lab filesystem.
        if source_meta.dev() != dest_meta.dev() {
            return Err(contextual(
                source_path,
                fail(io::Error::from_raw_os_error(libc::EXDEV)),
            ));
        }

        match reflink(&source_file, &dest_file) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL)
                ) =>
            {
                if let Err(copy_err) = copy_bounded(&source_file, &mut dest_file, max_bytes) {
                    return Err(contextual(&dest_path, fail(copy_err)));
                }
            }
            Err(err) => {
                if let Err(cleanup_err) = cleanup_dest(&vm_dir, file_name, &dest_path) {
                    return Err(contextual(&dest_path, with_cleanup(err, Err(cleanup_err))));
                }
                return Err(Error::Reflink {
                    path: dest_path.clone(),
                    source: err,
                });
            }
        }

        // A reflink clones the current length, so the bound is enforced here as well.
        let t_hash = Instant::now();
        let (sha256, size) = compute_file_sha256_and_size(&dest_file, max_bytes)
            .map_err(|e| contextual(&dest_path, fail(e)))?;
        let hash_ms = u64::try_from(t_hash.elapsed().as_millis()).unwrap_or(0);

        safe_unlink_source(&source_dir, &source_name, &source_meta).map_err(|e| Error::Io {
            path: source_path.into(),
            source: e,
        })?;

        Ok((
            FileMeta {
                path: PathBuf::from(format!("vms/{node}/{file_name}")),
                sha256,
                size,
            },
            hash_ms,
        ))
    }

    /// Writes the snapshot manifest to `manifest.json` atomically.
    ///
    /// # Errors
    /// Returns [`Error::Io`] or [`Error::Spec`].
    pub fn write_manifest(&self, manifest: &SnapshotManifest) -> Result<(), Error> {
        let json = manifest.to_json_string_pretty()?;
        let manifest_path = self.manifest_path();
        let tmp_name = "manifest.json.tmp";
        let tmp_path = self.root.join(tmp_name);
        let mut file = open(
            &self.directory,
            tmp_name.as_ref(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC,
        )
        .map_err(|e| contextual(&tmp_path, e))?;
        file.write_all(json.as_bytes())
            .map_err(|e| contextual(&tmp_path, e))?;
        file.sync_all().map_err(|e| contextual(&tmp_path, e))?;
        drop(file);
        renameat(
            &self.directory,
            Path::new(tmp_name),
            &self.directory,
            Path::new("manifest.json"),
        )
        .map_err(|e| contextual(&manifest_path, e.into()))?;
        Ok(())
    }

    /// Commits the snapshot, transferring ownership to durable storage.
    pub fn commit(&mut self) {
        self.owned = false;
    }

    /// Removes the snapshot directory.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if removal fails.
    pub fn remove(&mut self) -> Result<(), Error> {
        if self.owned {
            remove_owned(&self.parent, &self.directory, &self.id)
                .map_err(|e| contextual(&self.root, e))?;
            self.owned = false;
        }
        Ok(())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            tracing::error!(lab = %self.lab_id, snapshot = %self.id, %error, "partial snapshot cleanup failed");
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

    #[must_use]
    pub fn snapshot_name(lab_id: &str, snapshot_id: &str) -> String {
        format!("{lab_id}/{snapshot_id}")
    }

    #[must_use]
    pub fn snapshot_dir(&self, lab_id: &str, snapshot_id: &str) -> PathBuf {
        self.base.join("snapshots").join(lab_id).join(snapshot_id)
    }

    /// Creates a snapshot directory layout exclusively.
    ///
    /// # Errors
    /// Rejects invalid ids, existing snapshots, symlinks.
    pub fn create_snapshot(&self, lab_id: &str, snapshot_id: &str) -> Result<Snapshot, Error> {
        valid_id(lab_id)?;
        valid_id(snapshot_id)?;
        let snapshots_base = self.base.join("snapshots");
        let snapshots_parent =
            directory(&snapshots_base, true).map_err(|e| contextual(&snapshots_base, e))?;

        let lab_dir = match open(&snapshots_parent, lab_id.as_ref(), dirs()) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                make_directory(&snapshots_parent, lab_id)
                    .map_err(|e| contextual(&snapshots_base.join(lab_id), e))?
            }
            Err(e) => return Err(contextual(&snapshots_base.join(lab_id), e)),
        };

        let root = self.snapshot_dir(lab_id, snapshot_id);

        mkdirat(&lab_dir, snapshot_id, Mode::from_bits_truncate(0o700))
            .map_err(|e| contextual(&root, e.into()))?;

        let pinned = match open(&lab_dir, snapshot_id.as_ref(), dirs()) {
            Ok(file) => file,
            Err(error) => {
                unlinkat(&lab_dir, snapshot_id, UnlinkatFlags::RemoveDir)
                    .map_err(|e| contextual(&root, e.into()))?;
                return Err(contextual(&root, error));
            }
        };

        let mut snapshot = Snapshot {
            root,
            parent: lab_dir,
            directory: pinned,
            lab_id: lab_id.into(),
            id: snapshot_id.into(),
            owned: true,
        };

        if let Err(err) = make_directory(&snapshot.directory, "vms") {
            if let Err(remove_err) = snapshot.remove() {
                tracing::error!(%remove_err, "failed to clean up snapshot after vms directory failure");
            }
            return Err(contextual(&snapshot.root.join("vms"), err));
        }

        Ok(snapshot)
    }

    /// Removes a snapshot directory by name.
    ///
    /// # Errors
    /// Returns [`Error::Io`] or [`Error::InvalidId`].
    pub fn remove_snapshot(&self, lab_id: &str, snapshot_id: &str) -> Result<(), Error> {
        valid_id(lab_id)?;
        valid_id(snapshot_id)?;
        let snap_path = self.snapshot_dir(lab_id, snapshot_id);
        let lab_base = self.base.join("snapshots").join(lab_id);
        let result = (|| {
            let parent = directory(&lab_base, false)?;
            remove_entry(&parent, snapshot_id.as_ref())
        })();
        match result {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other.map_err(|e| contextual(&snap_path, e)),
        }
    }
}

fn compute_file_sha256_and_size(file: &File, max_bytes: u64) -> io::Result<(String, u64)> {
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total_size = 0u64;
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        total_size = total_size
            .checked_add(n as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file size overflow"))?;
        if total_size > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("file exceeded maximum allowed bound of {max_bytes} bytes during hashing"),
            ));
        }
        hasher.update(&buffer[..n]);
    }
    let sha256 = format!("{:02x}", hasher.finalize());
    Ok((sha256, total_size))
}

fn copy_bounded(source: &File, dest: &mut File, max_bytes: u64) -> io::Result<()> {
    let mut reader = source.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    dest.seek(SeekFrom::Start(0))?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        copied = copied
            .checked_add(n as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "copy size overflow"))?;
        if copied > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "source file exceeded maximum allowed bound of {max_bytes} bytes during copy"
                ),
            ));
        }
        dest.write_all(&buffer[..n])?;
    }
    dest.sync_all()?;
    Ok(())
}

fn safe_unlink_source(
    source_dir: &File,
    source_name: &std::ffi::OsStr,
    source_meta: &std::fs::Metadata,
) -> Result<(), io::Error> {
    let stat = nix::sys::stat::fstatat(
        source_dir,
        source_name,
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    );
    match stat {
        Ok(s) => {
            #[allow(clippy::useless_conversion)]
            let (ino, dev) = (u64::from(s.st_ino), u64::from(s.st_dev));
            if ino == source_meta.ino() && dev == source_meta.dev() {
                match unlinkat(source_dir, source_name, UnlinkatFlags::NoRemoveDir) {
                    Ok(()) | Err(Errno::ENOENT) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            } else {
                // Inode was replaced; do not unlink the replacement
                Ok(())
            }
        }
        Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Keeps the operation error primary and appends the cleanup failure, if any.
fn with_cleanup(err: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => err,
        Err(c_err) => io::Error::new(err.kind(), format!("{err}; additionally {c_err}")),
    }
}

fn cleanup_dest(vm_dir: &File, file_name: &str, dest_path: &Path) -> Result<(), io::Error> {
    match unlinkat(vm_dir, file_name, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(io::Error::other(format!(
            "failed to clean up partial destination {}: {e}",
            dest_path.display()
        ))),
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
/// Safely opens a directory by pinning every ancestor with `O_NOFOLLOW`.
///
/// # Errors
/// Returns [`std::io::Error`] if any component cannot be opened or is not a directory.
pub fn directory(path: &Path, create: bool) -> io::Result<File> {
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
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;

#[repr(C)]
#[derive(Debug, Default)]
struct open_how {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;

/// Descriptor-relative snapshot access under operator-controlled directories.
/// Ancestors and snapshot trees must not be writable by hostile processes: the
/// fallback cannot confine directories renamed outside the tree after opening.
/// Hard links and bind mounts are outside this protection. Consume returned
/// descriptors rather than reopening joined paths after verification.
pub struct SnapshotOpener {
    base: File,
}

impl SnapshotOpener {
    /// Creates a new `SnapshotOpener` rooted at the given directory path.
    ///
    /// # Errors
    /// Returns an error if the path cannot be opened safely.
    pub fn new(path: &Path) -> io::Result<Self> {
        let base = directory(path, false)?;
        Ok(Self { base })
    }

    /// Opens a file relative to the snapshot directory.
    ///
    /// # Errors
    /// Returns an error for absolute or parent paths, symlinks, missing files,
    /// or non-regular leaves.
    pub fn open(&self, path: &Path) -> io::Result<File> {
        self.open_impl(path, None, &mut |_| {})
    }

    /// Forces a resolver and pauses immediately before each resolution step.
    ///
    /// # Errors
    /// Returns the same errors as `open`, including ENOSYS for forced openat2.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn open_with_test_hook(
        &self,
        path: &Path,
        fallback: bool,
        mut before_step: impl FnMut(usize),
    ) -> io::Result<File> {
        self.open_impl(path, Some(fallback), &mut before_step)
    }

    fn open_impl(
        &self,
        path: &Path,
        fallback: Option<bool>,
        before_step: &mut dyn FnMut(usize),
    ) -> io::Result<File> {
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }

        if fallback != Some(true) {
            before_step(0);
            let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;

            let how = open_how {
                flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
                mode: 0,
                resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
            };

            // SAFETY: The base descriptor, NUL-terminated path, and initialized C-layout
            // how remain live for this syscall; the supplied size matches how.
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    self.base.as_raw_fd(),
                    path_c.as_ptr(),
                    &how,
                    std::mem::size_of::<open_how>(),
                )
            };

            if fd >= 0 {
                let fd_i32 = i32::try_from(fd)
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::Other))?;
                // SAFETY: We exclusively own this new descriptor.
                let file = unsafe { std::fs::File::from_raw_fd(fd_i32) };
                return regular_snapshot_file(file);
            }

            let err = io::Error::last_os_error();
            if fallback == Some(false) || err.raw_os_error() != Some(libc::ENOSYS) {
                return Err(err);
            }
        }
        let mut parent = self.base.try_clone()?;
        let mut step = 0;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            let name = match component {
                Component::Normal(name) => name,
                Component::CurDir => continue,
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            };
            let is_last = components.peek().is_none();
            let flags = if is_last {
                OFlag::O_RDONLY | OFlag::O_NONBLOCK
            } else {
                OFlag::O_RDONLY | OFlag::O_DIRECTORY
            };
            before_step(step);
            step += 1;
            parent = open(&parent, name, flags)?;
        }
        regular_snapshot_file(parent)
    }

    /// Returns a closure suitable for `SnapshotManifest::verify`.
    pub fn verify_opener(&self) -> impl FnMut(&Path) -> io::Result<File> + '_ {
        move |path| self.open(path)
    }
}

fn regular_snapshot_file(file: File) -> io::Result<File> {
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(file)
}

// No public path can make the destination cleanup fail after the destination was created.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_failure_is_appended_to_the_operation_error() {
        let operation = io::Error::from_raw_os_error(libc::EXDEV);
        let cleanup = Err(io::Error::from_raw_os_error(libc::EACCES));
        let combined = with_cleanup(operation, cleanup);
        assert_eq!(combined.kind(), io::ErrorKind::CrossesDevices);
        let message = combined.to_string();
        assert!(message.contains("cross-device"), "{message}");
        assert!(
            message.contains("; additionally Permission denied"),
            "{message}"
        );

        let alone = with_cleanup(io::Error::from_raw_os_error(libc::EXDEV), Ok(()));
        assert!(!alone.to_string().contains("additionally"));
    }
}

//! Request bodies of the Firecracker v1.17.0 VM state and snapshot routes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    Paused,
    Resumed,
}

/// Body of `PATCH /vm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vm {
    pub state: VmState,
}

impl Vm {
    #[must_use]
    pub const fn new(state: VmState) -> Self {
        Self { state }
    }

    #[must_use]
    pub const fn pause() -> Self {
        Self {
            state: VmState::Paused,
        }
    }

    #[must_use]
    pub const fn resume() -> Self {
        Self {
            state: VmState::Resumed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotType {
    Full,
    Diff,
}

/// Body of `PUT /snapshot/create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCreateParams {
    pub mem_file_path: PathBuf,
    pub snapshot_path: PathBuf,
    /// Firecracker defaults to `Full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_type: Option<SnapshotType>,
    /// Firecracker defaults to `true` (fsync before returning). `false` is faster but
    /// the files may not survive a host crash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_snapshot_files: Option<bool>,
}

impl SnapshotCreateParams {
    #[must_use]
    pub fn new(snapshot_path: impl Into<PathBuf>, mem_file_path: impl Into<PathBuf>) -> Self {
        Self {
            mem_file_path: mem_file_path.into(),
            snapshot_path: snapshot_path.into(),
            snapshot_type: None,
            sync_snapshot_files: None,
        }
    }

    #[must_use]
    pub fn full(snapshot_path: impl Into<PathBuf>, mem_file_path: impl Into<PathBuf>) -> Self {
        Self {
            mem_file_path: mem_file_path.into(),
            snapshot_path: snapshot_path.into(),
            snapshot_type: Some(SnapshotType::Full),
            sync_snapshot_files: None,
        }
    }

    #[must_use]
    pub const fn with_sync_snapshot_files(mut self, sync: bool) -> Self {
        self.sync_snapshot_files = Some(sync);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryBackendType {
    File,
    Uffd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBackend {
    /// The memory file for `File`; for `Uffd`, the Unix socket of the process that
    /// serves the guest page faults.
    pub backend_path: PathBuf,
    pub backend_type: MemoryBackendType,
}

impl MemoryBackend {
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            backend_path: path.into(),
            backend_type: MemoryBackendType::File,
        }
    }

    #[must_use]
    pub fn uffd(path: impl Into<PathBuf>) -> Self {
        Self {
            backend_path: path.into(),
            backend_type: MemoryBackendType::Uffd,
        }
    }
}

/// Attaches a restored interface to a different host TAP device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkOverride {
    pub host_dev_name: String,
    pub iface_id: String,
}

impl NetworkOverride {
    #[must_use]
    pub fn new(iface_id: impl Into<String>, host_dev_name: impl Into<String>) -> Self {
        Self {
            host_dev_name: host_dev_name.into(),
            iface_id: iface_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VsockOverride {
    pub uds_path: PathBuf,
}

impl VsockOverride {
    #[must_use]
    pub fn new(uds_path: impl Into<PathBuf>) -> Self {
        Self {
            uds_path: uds_path.into(),
        }
    }
}

/// `Snapshot`, the default, reuses the setting stored in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HugePagesConfig {
    Snapshot,
    None,
    Transparent,
    /// Requires the `Uffd` memory backend.
    #[serde(rename = "2M")]
    HugePages2M,
}

/// Body of `PUT /snapshot/load`.
///
/// The deprecated `mem_file_path` and `enable_diff_snapshots` are left out. Firecracker needs
/// exactly one of `mem_file_path` and `mem_backend`, so `mem_backend` is required here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotLoadParams {
    pub snapshot_path: PathBuf,
    pub mem_backend: MemoryBackend,
    /// Dylos leaves it unset: all VMs of a lab are resumed together after loading (ADR-0001).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_vm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_overrides: Option<Vec<NetworkOverride>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock_override: Option<VsockOverride>,
    /// x86 only. `true` advances the guest clock by the time elapsed since the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_realtime: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub huge_pages: Option<HugePagesConfig>,
}

impl SnapshotLoadParams {
    #[must_use]
    pub fn new(snapshot_path: impl Into<PathBuf>, mem_backend: MemoryBackend) -> Self {
        Self {
            snapshot_path: snapshot_path.into(),
            mem_backend,
            resume_vm: None,
            network_overrides: None,
            track_dirty_pages: None,
            vsock_override: None,
            clock_realtime: None,
            huge_pages: None,
        }
    }

    #[must_use]
    pub fn with_file_backend(
        snapshot_path: impl Into<PathBuf>,
        mem_backend_path: impl Into<PathBuf>,
    ) -> Self {
        Self::new(snapshot_path, MemoryBackend::file(mem_backend_path))
    }

    #[must_use]
    pub const fn with_resume_vm(mut self, resume: bool) -> Self {
        self.resume_vm = Some(resume);
        self
    }

    #[must_use]
    pub fn with_network_overrides(mut self, overrides: Vec<NetworkOverride>) -> Self {
        self.network_overrides = Some(overrides);
        self
    }
}

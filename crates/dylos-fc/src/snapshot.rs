use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `OpenAPI` definition: `Vm::state`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    Paused,
    Resumed,
}

/// `OpenAPI` definition: `Vm`
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

/// `OpenAPI` definition: `SnapshotCreateParams::snapshot_type`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotType {
    Full,
    Diff,
}

/// `OpenAPI` definition: `SnapshotCreateParams`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCreateParams {
    pub mem_file_path: PathBuf,
    pub snapshot_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_type: Option<SnapshotType>,
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

/// `OpenAPI` definition: `MemoryBackend::backend_type`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryBackendType {
    File,
    Uffd,
}

/// `OpenAPI` definition: `MemoryBackend`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBackend {
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

/// `OpenAPI` definition: `NetworkOverride`
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

/// `OpenAPI` definition: `VsockOverride`
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

/// `OpenAPI` definition: `SnapshotLoadParams::huge_pages`
///
/// Note: Setting this to [`HugePagesConfig::HugePages2M`] requires the `Uffd` memory backend per the Firecracker specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HugePagesConfig {
    Snapshot,
    None,
    Transparent,
    /// Requires the `Uffd` memory backend per the Firecracker specification.
    #[serde(rename = "2M")]
    HugePages2M,
}

/// `OpenAPI` definition: `SnapshotLoadParams`
///
/// Deprecated fields `enable_diff_snapshots` and `mem_file_path` are omitted.
/// Firecracker requires exactly one memory backend parameter on snapshot load;
/// since deprecated `mem_file_path` is omitted, `mem_backend` is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotLoadParams {
    pub snapshot_path: PathBuf,
    pub mem_backend: MemoryBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_vm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_overrides: Option<Vec<NetworkOverride>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock_override: Option<VsockOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_realtime: Option<bool>,
    /// Note: [`HugePagesConfig::HugePages2M`] requires the `Uffd` memory backend per the Firecracker specification.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_vm_serialization() -> Result<(), serde_json::Error> {
        let pause = Vm::pause();
        assert_eq!(serde_json::to_value(&pause)?, json!({"state": "Paused"}));
        let resume = Vm::resume();
        assert_eq!(serde_json::to_value(&resume)?, json!({"state": "Resumed"}));

        let deserialized: Vm = serde_json::from_str(r#"{"state":"Paused"}"#)?;
        assert_eq!(deserialized, pause);
        Ok(())
    }

    #[test]
    fn test_snapshot_create_params_serialization() -> Result<(), serde_json::Error> {
        let full = SnapshotCreateParams::full("/snap", "/mem").with_sync_snapshot_files(true);
        assert_eq!(
            serde_json::to_value(&full)?,
            json!({
                "mem_file_path": "/mem",
                "snapshot_path": "/snap",
                "snapshot_type": "Full",
                "sync_snapshot_files": true
            })
        );
        let deserialized_full: SnapshotCreateParams =
            serde_json::from_str(&serde_json::to_string(&full)?)?;
        assert_eq!(deserialized_full, full);

        let diff = SnapshotCreateParams {
            mem_file_path: PathBuf::from("/diff_mem"),
            snapshot_path: PathBuf::from("/diff_snap"),
            snapshot_type: Some(SnapshotType::Diff),
            sync_snapshot_files: Some(false),
        };
        assert_eq!(
            serde_json::to_value(&diff)?,
            json!({
                "mem_file_path": "/diff_mem",
                "snapshot_path": "/diff_snap",
                "snapshot_type": "Diff",
                "sync_snapshot_files": false
            })
        );
        let deserialized_diff: SnapshotCreateParams =
            serde_json::from_str(&serde_json::to_string(&diff)?)?;
        assert_eq!(deserialized_diff, diff);

        let minimal = SnapshotCreateParams::new("/snap", "/mem");
        let val = serde_json::to_value(&minimal)?;
        assert_eq!(
            val,
            json!({ "mem_file_path": "/mem", "snapshot_path": "/snap" })
        );
        assert!(val.get("snapshot_type").is_none());
        assert!(val.get("sync_snapshot_files").is_none());
        let deserialized_minimal: SnapshotCreateParams =
            serde_json::from_str(&serde_json::to_string(&minimal)?)?;
        assert_eq!(deserialized_minimal, minimal);
        Ok(())
    }

    #[test]
    fn test_snapshot_create_params_roundtrip() -> Result<(), serde_json::Error> {
        let params = SnapshotCreateParams::full("/var/lib/dylos/snap", "/var/lib/dylos/mem")
            .with_sync_snapshot_files(true);
        let serialized = serde_json::to_string(&params)?;
        let deserialized: SnapshotCreateParams = serde_json::from_str(&serialized)?;
        assert_eq!(deserialized, params);
        Ok(())
    }

    #[test]
    fn test_backend_and_overrides_serialization() -> Result<(), serde_json::Error> {
        let file = MemoryBackend::file("/mem");
        assert_eq!(
            serde_json::to_value(&file)?,
            json!({ "backend_path": "/mem", "backend_type": "File" })
        );

        let uffd = MemoryBackend::uffd("/sock");
        assert_eq!(
            serde_json::to_value(&uffd)?,
            json!({ "backend_path": "/sock", "backend_type": "Uffd" })
        );

        let net = NetworkOverride::new("eth0", "tap0");
        assert_eq!(
            serde_json::to_value(&net)?,
            json!({ "host_dev_name": "tap0", "iface_id": "eth0" })
        );

        let vsock = VsockOverride::new("/vsock.sock");
        assert_eq!(
            serde_json::to_value(&vsock)?,
            json!({ "uds_path": "/vsock.sock" })
        );
        Ok(())
    }

    #[test]
    fn test_snapshot_load_params_serialization() -> Result<(), serde_json::Error> {
        let minimal = SnapshotLoadParams::with_file_backend("/snap", "/mem");
        let val = serde_json::to_value(&minimal)?;
        assert_eq!(
            val,
            json!({
                "snapshot_path": "/snap",
                "mem_backend": { "backend_path": "/mem", "backend_type": "File" }
            })
        );
        assert!(val.get("resume_vm").is_none());
        assert!(val.get("network_overrides").is_none());
        assert!(val.get("track_dirty_pages").is_none());
        assert!(val.get("vsock_override").is_none());
        assert!(val.get("clock_realtime").is_none());
        assert!(val.get("huge_pages").is_none());
        assert!(val.get("mem_file_path").is_none());
        assert!(val.get("enable_diff_snapshots").is_none());

        let deserialized_minimal: SnapshotLoadParams =
            serde_json::from_str(&serde_json::to_string(&minimal)?)?;
        assert_eq!(deserialized_minimal, minimal);

        let loaded = SnapshotLoadParams::with_file_backend("/snap", "/mem")
            .with_resume_vm(true)
            .with_network_overrides(vec![NetworkOverride::new("eth0", "tap0")]);
        assert_eq!(
            serde_json::to_value(&loaded)?,
            json!({
                "snapshot_path": "/snap",
                "mem_backend": { "backend_path": "/mem", "backend_type": "File" },
                "resume_vm": true,
                "network_overrides": [{ "host_dev_name": "tap0", "iface_id": "eth0" }]
            })
        );
        let deserialized_loaded: SnapshotLoadParams =
            serde_json::from_str(&serde_json::to_string(&loaded)?)?;
        assert_eq!(deserialized_loaded, loaded);

        let full = SnapshotLoadParams {
            snapshot_path: PathBuf::from("/snap"),
            mem_backend: MemoryBackend::uffd("/sock"),
            resume_vm: Some(false),
            network_overrides: Some(vec![
                NetworkOverride::new("eth0", "tap0"),
                NetworkOverride::new("eth1", "tap1"),
            ]),
            track_dirty_pages: Some(true),
            vsock_override: Some(VsockOverride::new("/vsock.sock")),
            clock_realtime: Some(true),
            huge_pages: Some(HugePagesConfig::HugePages2M),
        };
        assert_eq!(
            serde_json::to_value(&full)?,
            json!({
                "snapshot_path": "/snap",
                "mem_backend": { "backend_path": "/sock", "backend_type": "Uffd" },
                "resume_vm": false,
                "network_overrides": [
                    { "host_dev_name": "tap0", "iface_id": "eth0" },
                    { "host_dev_name": "tap1", "iface_id": "eth1" }
                ],
                "track_dirty_pages": true,
                "vsock_override": { "uds_path": "/vsock.sock" },
                "clock_realtime": true,
                "huge_pages": "2M"
            })
        );
        let deserialized_full: SnapshotLoadParams =
            serde_json::from_str(&serde_json::to_string(&full)?)?;
        assert_eq!(deserialized_full, full);
        Ok(())
    }

    #[test]
    fn test_snapshot_load_params_roundtrip() -> Result<(), serde_json::Error> {
        let params = SnapshotLoadParams {
            snapshot_path: PathBuf::from("/snap"),
            mem_backend: MemoryBackend::uffd("/sock"),
            resume_vm: Some(false),
            network_overrides: Some(vec![
                NetworkOverride::new("eth0", "tap0"),
                NetworkOverride::new("eth1", "tap1"),
            ]),
            track_dirty_pages: Some(true),
            vsock_override: Some(VsockOverride::new("/vsock.sock")),
            clock_realtime: Some(true),
            huge_pages: Some(HugePagesConfig::HugePages2M),
        };
        let serialized = serde_json::to_string(&params)?;
        let deserialized: SnapshotLoadParams = serde_json::from_str(&serialized)?;
        assert_eq!(deserialized, params);
        Ok(())
    }
}

//! Request bodies of the Firecracker v1.17.0 configuration routes.

use serde::{Deserialize, Serialize};

/// Body of `PUT /machine-config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfiguration {
    /// 1 or an even number, up to 32.
    pub vcpu_count: u8,
    pub mem_size_mib: usize,
    /// x86 only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    /// Required for diff snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
}

impl MachineConfiguration {
    #[must_use]
    pub const fn new(vcpu_count: u8, mem_size_mib: usize) -> Self {
        Self {
            vcpu_count,
            mem_size_mib,
            smt: None,
            track_dirty_pages: None,
        }
    }
}

/// Body of `PUT /boot-source`. Paths are on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
}

impl BootSource {
    #[must_use]
    pub fn new(kernel_image_path: impl Into<String>) -> Self {
        Self {
            kernel_image_path: kernel_image_path.into(),
            boot_args: None,
            initrd_path: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheType {
    /// Guest flushes are not forwarded to the host.
    Unsafe,
    Writeback,
}

/// Only the synchronous engine is used in the spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoEngine {
    Sync,
}

/// Body of `PUT /drives/{drive_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub is_root_device: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_on_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_read_only: Option<bool>,
    /// Only valid on the root device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partuuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_type: Option<CacheType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_engine: Option<IoEngine>,
}

impl Drive {
    #[must_use]
    pub fn new(drive_id: impl Into<String>, is_root_device: bool) -> Self {
        Self {
            drive_id: drive_id.into(),
            is_root_device,
            path_on_host: None,
            is_read_only: None,
            partuuid: None,
            cache_type: None,
            io_engine: None,
        }
    }
}

/// Body of `PUT /network-interfaces/{iface_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    /// Name of the host TAP device.
    pub host_dev_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
    /// Advertised to the guest through `VIRTIO_NET_F_MTU`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

impl NetworkInterface {
    #[must_use]
    pub fn new(iface_id: impl Into<String>, host_dev_name: impl Into<String>) -> Self {
        Self {
            iface_id: iface_id.into(),
            host_dev_name: host_dev_name.into(),
            guest_mac: None,
            mtu: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionType {
    FlushMetrics,
    InstanceStart,
    SendCtrlAltDel,
}

/// Body of `PUT /actions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceActionInfo {
    pub action_type: ActionType,
}

impl InstanceActionInfo {
    #[must_use]
    pub const fn new(action_type: ActionType) -> Self {
        Self { action_type }
    }

    #[must_use]
    pub const fn instance_start() -> Self {
        Self::new(ActionType::InstanceStart)
    }
}

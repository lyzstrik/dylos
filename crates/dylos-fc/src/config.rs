//! Serde types for Firecracker microVM configuration routes.
//!
//! These types correspond to the Firecracker `OpenAPI` v1.17.0 specification for
//! pre-boot and boot-time configuration routes:
//! - `PUT /machine-config` ([`MachineConfiguration`])
//! - `PUT /boot-source` ([`BootSource`])
//! - `PUT /drives/{drive_id}` ([`Drive`])
//! - `PUT /network-interfaces/{iface_id}` ([`NetworkInterface`])
//! - `PUT /actions` ([`InstanceActionInfo`])

use serde::{Deserialize, Serialize};

/// Machine configuration of the microVM.
///
/// Corresponds to `MachineConfiguration` definition in Firecracker `OpenAPI` spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfiguration {
    /// Number of vCPUs (either 1 or an even number, up to 32).
    pub vcpu_count: u8,
    /// Memory size of the VM in MiB.
    pub mem_size_mib: usize,
    /// Flag for enabling/disabling simultaneous multithreading (x86 only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    /// Enable dirty page tracking for incremental/diff snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
}

impl MachineConfiguration {
    /// Creates a new `MachineConfiguration` with the required fields.
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

/// Boot source descriptor for the guest VM.
///
/// Corresponds to `BootSource` definition in Firecracker `OpenAPI` spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootSource {
    /// Host-level path to the kernel image used to boot the guest.
    pub kernel_image_path: String,
    /// Kernel boot arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_args: Option<String>,
    /// Host-level path to the initrd image used to boot the guest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
}

impl BootSource {
    /// Creates a new `BootSource` with the required kernel image path.
    #[must_use]
    pub fn new(kernel_image_path: impl Into<String>) -> Self {
        Self {
            kernel_image_path: kernel_image_path.into(),
            boot_args: None,
            initrd_path: None,
        }
    }
}

/// Caching strategy for a block device.
///
/// Corresponds to property `cache_type` in `Drive` (`OpenAPI` spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheType {
    /// Unsafe caching (writeback without host sync).
    Unsafe,
    /// Writeback caching.
    Writeback,
}

/// IO engine used by the drive device.
///
/// Corresponds to property `io_engine` in `Drive` (`OpenAPI` spec).
/// Only the synchronous engine is supported for the spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoEngine {
    /// Synchronous IO engine.
    Sync,
}

/// Drive configuration for the microVM.
///
/// Corresponds to `Drive` definition in Firecracker `OpenAPI` spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    /// Unique identifier of the drive.
    pub drive_id: String,
    /// Whether this drive is the root device.
    pub is_root_device: bool,
    /// Host-level path for the guest drive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_on_host: Option<String>,
    /// Whether the drive is read-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_read_only: Option<bool>,
    /// Unique ID of the boot partition (only valid if `is_root_device` is true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partuuid: Option<String>,
    /// Caching strategy for the drive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_type: Option<CacheType>,
    /// IO engine type used by the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_engine: Option<IoEngine>,
}

impl Drive {
    /// Creates a new `Drive` with the required fields.
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

/// Network interface configuration for the microVM.
///
/// Corresponds to `NetworkInterface` definition in Firecracker `OpenAPI` spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    /// Interface identifier.
    pub iface_id: String,
    /// Host-level device name for the guest network interface (e.g. TAP device name).
    pub host_dev_name: String,
    /// MAC address advertised to the guest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
    /// MTU to advertise to the guest via `VIRTIO_NET_F_MTU`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

impl NetworkInterface {
    /// Creates a new `NetworkInterface` with the required fields.
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

/// Action types supported by Firecracker's `/actions` route.
///
/// Corresponds to property `action_type` in `InstanceActionInfo` (`OpenAPI` spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionType {
    /// Flushes microVM metrics.
    FlushMetrics,
    /// Starts the microVM instance.
    InstanceStart,
    /// Sends Ctrl+Alt+Del to the guest.
    SendCtrlAltDel,
}

/// Synchronous instance action request.
///
/// Corresponds to `InstanceActionInfo` definition in Firecracker `OpenAPI` spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceActionInfo {
    /// Action type to execute.
    pub action_type: ActionType,
}

impl InstanceActionInfo {
    /// Creates a new `InstanceActionInfo` for the given action type.
    #[must_use]
    pub const fn new(action_type: ActionType) -> Self {
        Self { action_type }
    }

    /// Creates an `InstanceActionInfo` configured to start the microVM.
    #[must_use]
    pub const fn instance_start() -> Self {
        Self::new(ActionType::InstanceStart)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_absent_keys(val: &serde_json::Value, keys: &[&str]) {
        for &k in keys {
            assert!(val.get(k).is_none(), "key {k} should be omitted");
        }
    }

    #[test]
    fn machine_config_serialization() -> Result<(), serde_json::Error> {
        let minimal = MachineConfiguration::new(2, 1024);
        let min_val = serde_json::to_value(&minimal)?;
        assert_eq!(min_val, json!({ "vcpu_count": 2, "mem_size_mib": 1024 }));
        assert_absent_keys(&min_val, &["smt", "track_dirty_pages"]);
        assert_eq!(
            serde_json::from_value::<MachineConfiguration>(min_val)?,
            minimal
        );

        let full = MachineConfiguration {
            vcpu_count: 4,
            mem_size_mib: 2048,
            smt: Some(true),
            track_dirty_pages: Some(true),
        };
        let full_val = serde_json::to_value(&full)?;
        assert_eq!(
            full_val,
            json!({
                "vcpu_count": 4,
                "mem_size_mib": 2048,
                "smt": true,
                "track_dirty_pages": true
            })
        );
        assert_eq!(
            serde_json::from_value::<MachineConfiguration>(full_val)?,
            full
        );
        Ok(())
    }

    #[test]
    fn boot_source_serialization() -> Result<(), serde_json::Error> {
        let minimal = BootSource::new("/srv/dylos/vmlinux.bin");
        let min_val = serde_json::to_value(&minimal)?;
        assert_eq!(
            min_val,
            json!({ "kernel_image_path": "/srv/dylos/vmlinux.bin" })
        );
        assert_absent_keys(&min_val, &["boot_args", "initrd_path"]);
        assert_eq!(serde_json::from_value::<BootSource>(min_val)?, minimal);

        let full = BootSource {
            kernel_image_path: "/srv/dylos/vmlinux.bin".to_owned(),
            boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_owned()),
            initrd_path: Some("/srv/dylos/initrd.img".to_owned()),
        };
        let full_val = serde_json::to_value(&full)?;
        assert_eq!(
            full_val,
            json!({
                "kernel_image_path": "/srv/dylos/vmlinux.bin",
                "boot_args": "console=ttyS0 reboot=k panic=1 pci=off",
                "initrd_path": "/srv/dylos/initrd.img"
            })
        );
        assert_eq!(serde_json::from_value::<BootSource>(full_val)?, full);
        Ok(())
    }

    #[test]
    fn drive_serialization() -> Result<(), serde_json::Error> {
        let minimal = Drive::new("rootfs", true);
        let min_val = serde_json::to_value(&minimal)?;
        assert_eq!(
            min_val,
            json!({ "drive_id": "rootfs", "is_root_device": true })
        );
        assert_absent_keys(
            &min_val,
            &[
                "path_on_host",
                "is_read_only",
                "partuuid",
                "cache_type",
                "io_engine",
            ],
        );
        assert_eq!(serde_json::from_value::<Drive>(min_val)?, minimal);

        let full = Drive {
            drive_id: "rootfs".to_owned(),
            is_root_device: true,
            path_on_host: Some("/srv/dylos/disks/rootfs.ext4".to_owned()),
            is_read_only: Some(false),
            partuuid: Some("00000000-0000-0000-0000-000000000001".to_owned()),
            cache_type: Some(CacheType::Unsafe),
            io_engine: Some(IoEngine::Sync),
        };
        let full_val = serde_json::to_value(&full)?;
        assert_eq!(
            full_val,
            json!({
                "drive_id": "rootfs",
                "is_root_device": true,
                "path_on_host": "/srv/dylos/disks/rootfs.ext4",
                "is_read_only": false,
                "partuuid": "00000000-0000-0000-0000-000000000001",
                "cache_type": "Unsafe",
                "io_engine": "Sync"
            })
        );
        assert_eq!(serde_json::from_value::<Drive>(full_val)?, full);
        Ok(())
    }

    #[test]
    fn network_interface_serialization() -> Result<(), serde_json::Error> {
        let minimal = NetworkInterface::new("net0", "tap-vm1-0");
        let min_val = serde_json::to_value(&minimal)?;
        assert_eq!(
            min_val,
            json!({ "iface_id": "net0", "host_dev_name": "tap-vm1-0" })
        );
        assert_absent_keys(&min_val, &["guest_mac", "mtu"]);
        assert_eq!(
            serde_json::from_value::<NetworkInterface>(min_val)?,
            minimal
        );

        let full = NetworkInterface {
            iface_id: "net0".to_owned(),
            host_dev_name: "tap-vm1-0".to_owned(),
            guest_mac: Some("AA:FC:00:00:00:01".to_owned()),
            mtu: Some(1500),
        };
        let full_val = serde_json::to_value(&full)?;
        assert_eq!(
            full_val,
            json!({
                "iface_id": "net0",
                "host_dev_name": "tap-vm1-0",
                "guest_mac": "AA:FC:00:00:00:01",
                "mtu": 1500
            })
        );
        assert_eq!(serde_json::from_value::<NetworkInterface>(full_val)?, full);
        Ok(())
    }

    #[test]
    fn instance_action_info_serialization() -> Result<(), serde_json::Error> {
        let action = InstanceActionInfo::instance_start();
        let serialized = serde_json::to_value(&action)?;
        assert_eq!(serialized, json!({ "action_type": "InstanceStart" }));
        assert_eq!(
            serde_json::from_value::<InstanceActionInfo>(serialized)?,
            action
        );

        for (act, name) in [
            (ActionType::FlushMetrics, "FlushMetrics"),
            (ActionType::SendCtrlAltDel, "SendCtrlAltDel"),
        ] {
            let info = InstanceActionInfo::new(act);
            let val = serde_json::to_value(&info)?;
            assert_eq!(val, json!({ "action_type": name }));
            assert_eq!(serde_json::from_value::<InstanceActionInfo>(val)?, info);
        }
        Ok(())
    }
}

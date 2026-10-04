//! JSON shape of the configuration types: minimal values omit optional keys, full values
//! serialize every field, and both round-trip.

use dylos_fc::config::{
    ActionType, BootSource, CacheType, Drive, InstanceActionInfo, IoEngine, MachineConfiguration,
    NetworkInterface,
};
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

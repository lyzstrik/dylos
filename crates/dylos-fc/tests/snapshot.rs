//! JSON shape of the VM state and snapshot types: optional keys are omitted when unset,
//! and every value round-trips.

use std::path::PathBuf;

use dylos_fc::snapshot::{
    HugePagesConfig, MemoryBackend, NetworkOverride, SnapshotCreateParams, SnapshotLoadParams,
    SnapshotType, Vm, VsockOverride,
};
use serde_json::json;

#[test]
fn vm_serialization() -> Result<(), serde_json::Error> {
    let pause = Vm::pause();
    assert_eq!(serde_json::to_value(&pause)?, json!({"state": "Paused"}));
    let resume = Vm::resume();
    assert_eq!(serde_json::to_value(&resume)?, json!({"state": "Resumed"}));

    let deserialized: Vm = serde_json::from_str(r#"{"state":"Paused"}"#)?;
    assert_eq!(deserialized, pause);
    Ok(())
}

#[test]
fn snapshot_create_params_serialization() -> Result<(), serde_json::Error> {
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
fn snapshot_create_params_roundtrip() -> Result<(), serde_json::Error> {
    let params = SnapshotCreateParams::full("/var/lib/dylos/snap", "/var/lib/dylos/mem")
        .with_sync_snapshot_files(true);
    let serialized = serde_json::to_string(&params)?;
    let deserialized: SnapshotCreateParams = serde_json::from_str(&serialized)?;
    assert_eq!(deserialized, params);
    Ok(())
}

#[test]
fn backend_and_overrides_serialization() -> Result<(), serde_json::Error> {
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
fn snapshot_load_params_serialization() -> Result<(), serde_json::Error> {
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
fn snapshot_load_params_roundtrip() -> Result<(), serde_json::Error> {
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

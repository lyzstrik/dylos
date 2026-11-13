use dylos_core::{
    LabSpec,
    manifest::{FileMeta, SnapshotManifest, StepDurations, VmManifest, sha256_hex},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
fn sha(data: &[u8]) -> String {
    sha256_hex(&Sha256::digest(data).into())
}

pub fn create_test_snapshot(
    dir: &Path,
    spec: LabSpec,
    disk_data_by_vm: &[(&str, &[u8])],
) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let mut vms = BTreeMap::new();

    for &(vm_name, disk_data) in disk_data_by_vm {
        let vm_dir = dir.join("vms").join(vm_name);
        fs::create_dir_all(&vm_dir).unwrap();

        let state_data = format!("state for {vm_name}").into_bytes();
        let mem_data = format!("memory for {vm_name} with extra padding").into_bytes();

        let state_path = format!("vms/{vm_name}/vm.state");
        let mem_path = format!("vms/{vm_name}/vm.mem");
        let disk_path = format!("vms/{vm_name}/rootfs.ext4");

        fs::write(dir.join(&state_path), &state_data).unwrap();
        fs::write(dir.join(&mem_path), &mem_data).unwrap();
        fs::write(dir.join(&disk_path), disk_data).unwrap();

        vms.insert(
            vm_name.to_string(),
            VmManifest {
                state_file: FileMeta {
                    path: PathBuf::from(&state_path),
                    size: state_data.len() as u64,
                    sha256: sha(&state_data),
                    fsverity: None,
                },
                memory_file: FileMeta {
                    path: PathBuf::from(&mem_path),
                    size: mem_data.len() as u64,
                    sha256: sha(&mem_data),
                    fsverity: None,
                },
                disks: vec![FileMeta {
                    path: PathBuf::from(&disk_path),
                    size: disk_data.len() as u64,
                    sha256: sha(disk_data),
                    fsverity: None,
                }],
            },
        );
    }

    let manifest = SnapshotManifest {
        parent: None,
        format_version: 1,
        lab_spec: spec,
        vms,
        firecracker_version: "1.17.0".into(),
        host_cpu_model: "test-cpu".into(),
        created_at_unix_ms: 1000,
        step_durations_ms: StepDurations {
            freeze: 1,
            pause: 2,
            snapshot: 3,
            resume: 4,
            thaw: 5,
        },
    };

    let manifest_json = manifest.to_json_string().unwrap();
    fs::write(dir.join("manifest.json"), manifest_json).unwrap();
    dir.to_path_buf()
}

pub fn single_vm_spec() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("../fixtures/diff-single.yaml")).unwrap()
}

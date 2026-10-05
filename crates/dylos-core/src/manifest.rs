use crate::{Error, LabSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotManifest {
    pub format_version: u32,
    pub lab_spec: LabSpec,
    pub vms: HashMap<String, VmManifest>,
    pub firecracker_version: String,
    pub host_cpu_model: String,
    pub created_at_unix_ms: u64,
    pub step_durations_ms: StepDurations,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepDurations {
    pub freeze: u64,
    pub pause: u64,
    pub snapshot: u64,
    pub resume: u64,
    pub thaw: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VmManifest {
    pub state_file: FileMeta,
    pub memory_file: FileMeta,
    pub disks: Vec<FileMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileMeta {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct VersionCheck {
    format_version: u32,
}

impl SnapshotManifest {
    #[allow(clippy::missing_errors_doc)]
    pub fn from_json_str(json: &str) -> Result<Self, Error> {
        let check: VersionCheck = serde_json::from_str(json)?;
        if check.format_version != 1 {
            return Err(Error::UnsupportedManifestVersion {
                found: check.format_version,
                supported: 1,
            });
        }

        let manifest: SnapshotManifest = serde_json::from_str(json)?;
        manifest.validate_paths()?;
        Ok(manifest)
    }

    #[allow(clippy::missing_errors_doc)]
    pub fn to_json_string(&self) -> Result<String, Error> {
        self.validate_paths()?;
        Ok(serde_json::to_string(self)?)
    }

    #[allow(clippy::missing_errors_doc)]
    pub fn to_json_string_pretty(&self) -> Result<String, Error> {
        self.validate_paths()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    #[allow(clippy::missing_errors_doc)]
    pub fn validate_paths(&self) -> Result<(), Error> {
        for (vm_name, vm_manifest) in &self.vms {
            let files = std::iter::once(&vm_manifest.state_file)
                .chain(std::iter::once(&vm_manifest.memory_file))
                .chain(vm_manifest.disks.iter());

            for file in files {
                Self::validate_single_path(vm_name, &file.path)?;
            }
        }
        Ok(())
    }

    fn validate_single_path(vm_name: &str, path: &Path) -> Result<(), Error> {
        if path.is_absolute() {
            return Err(Error::InvalidPath {
                vm: vm_name.to_string(),
                path: path.to_path_buf(),
            });
        }
        for component in path.components() {
            match component {
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(Error::InvalidPath {
                        vm: vm_name.to_string(),
                        path: path.to_path_buf(),
                    });
                }
                Component::Normal(_) | Component::CurDir => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::missing_errors_doc)]
    pub fn verify<R: Read>(
        &self,
        mut open: impl FnMut(&Path) -> std::io::Result<R>,
    ) -> Result<(), Error> {
        for (vm_name, vm_manifest) in &self.vms {
            let files = std::iter::once(&vm_manifest.state_file)
                .chain(std::iter::once(&vm_manifest.memory_file))
                .chain(vm_manifest.disks.iter());

            for file in files {
                let mut reader = open(&file.path).map_err(|e| Error::Io {
                    vm: vm_name.clone(),
                    path: file.path.clone(),
                    source: e,
                })?;

                let mut hasher = Sha256::new();
                let mut buffer = [0u8; 8192];
                let mut actual_size = 0u64;

                loop {
                    let n = reader.read(&mut buffer).map_err(|e| Error::Io {
                        vm: vm_name.clone(),
                        path: file.path.clone(),
                        source: e,
                    })?;
                    if n == 0 {
                        break;
                    }
                    actual_size += n as u64;
                    hasher.update(&buffer[..n]);
                }

                let actual_sha256 = format!("{:02x}", hasher.finalize());

                if actual_size != file.size || actual_sha256 != file.sha256 {
                    return Err(Error::IntegrityMismatch {
                        vm: vm_name.clone(),
                        path: file.path.clone(),
                        expected_size: file.size,
                        actual_size,
                        expected_sha256: file.sha256.clone(),
                        actual_sha256,
                    });
                }
            }
        }
        Ok(())
    }
}

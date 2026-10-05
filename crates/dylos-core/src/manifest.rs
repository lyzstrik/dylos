use crate::{Error, LabSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub format_version: u32,
    pub lab_spec: LabSpec,
    pub vms: BTreeMap<String, VmManifest>,
    pub firecracker_version: String,
    pub host_cpu_model: String,
    pub created_at_unix_ms: u64,
    pub step_durations_ms: StepDurations,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StepDurations {
    pub freeze: u64,
    pub pause: u64,
    pub snapshot: u64,
    pub resume: u64,
    pub thaw: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VmManifest {
    pub state_file: FileMeta,
    pub memory_file: FileMeta,
    pub disks: Vec<FileMeta>,
}

impl VmManifest {
    fn files(&self) -> impl Iterator<Item = &FileMeta> {
        std::iter::once(&self.state_file)
            .chain(std::iter::once(&self.memory_file))
            .chain(&self.disks)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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
    /// Deserializes and validates a [`SnapshotManifest`] from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Json`] if JSON parsing fails,
    /// [`Error::UnsupportedManifestVersion`] if `format_version` is not 1, or
    /// [`Error::InvalidPath`] if any VM file path is absolute or contains parent components.
    pub fn from_json_str(json: &str) -> Result<Self, Error> {
        // Read format_version before the full parse so an unknown version gives
        // UnsupportedManifestVersion instead of a field error.
        let check: VersionCheck = serde_json::from_str(json)?;
        if check.format_version != 1 {
            return Err(Error::UnsupportedManifestVersion {
                found: check.format_version,
                supported: 1,
            });
        }

        let manifest: Self = serde_json::from_str(json)?;
        manifest.validate_paths()?;
        Ok(manifest)
    }

    /// Serializes the [`SnapshotManifest`] to a compact JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if any VM file path is absolute or contains parent components,
    /// or [`Error::Json`] if serialization fails.
    pub fn to_json_string(&self) -> Result<String, Error> {
        self.validate_paths()?;
        Ok(serde_json::to_string(self)?)
    }

    /// Serializes the [`SnapshotManifest`] to a pretty-printed JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if any VM file path is absolute or contains parent components,
    /// or [`Error::Json`] if serialization fails.
    pub fn to_json_string_pretty(&self) -> Result<String, Error> {
        self.validate_paths()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Validates that all file paths in all VM manifests are safe relative paths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if any file path is absolute or contains parent components.
    pub fn validate_paths(&self) -> Result<(), Error> {
        for (vm_name, vm_manifest) in &self.vms {
            for file in vm_manifest.files() {
                Self::validate_single_path(vm_name, &file.path)?;
            }
        }
        Ok(())
    }

    fn validate_single_path(vm_name: &str, path: &Path) -> Result<(), Error> {
        // Paths are relative to the snapshot directory.
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

    /// Verifies the integrity of all VM files against their expected size and SHA-256 hash.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if opening or reading a file fails, or
    /// [`Error::IntegrityMismatch`] if a file's size or SHA-256 checksum does not match.
    pub fn verify<R: Read>(
        &self,
        mut open: impl FnMut(&Path) -> std::io::Result<R>,
    ) -> Result<(), Error> {
        // The reader is supplied by the caller to keep dylos-core free of file system access.
        for (vm_name, vm_manifest) in &self.vms {
            for file in vm_manifest.files() {
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

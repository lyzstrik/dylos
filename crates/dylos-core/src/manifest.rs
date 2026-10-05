use crate::{Error, LabSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[serde(deny_unknown_fields)]
struct RawSnapshotManifest {
    format_version: u32,
    lab_spec: LabSpec,
    vms: BTreeMap<String, VmManifest>,
    firecracker_version: String,
    host_cpu_model: String,
    created_at_unix_ms: u64,
    step_durations_ms: StepDurations,
}

impl<'de> Deserialize<'de> for SnapshotManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let version = value
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| serde::de::Error::missing_field("format_version"))?;

        if version != 1 {
            return Err(serde::de::Error::custom(format!(
                "unsupported manifest version {version}, only version 1 is supported"
            )));
        }

        let raw: RawSnapshotManifest =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;

        let manifest = SnapshotManifest {
            format_version: raw.format_version,
            lab_spec: raw.lab_spec,
            vms: raw.vms,
            firecracker_version: raw.firecracker_version,
            host_cpu_model: raw.host_cpu_model,
            created_at_unix_ms: raw.created_at_unix_ms,
            step_durations_ms: raw.step_durations_ms,
        };

        manifest
            .validate_paths()
            .map_err(|e| serde::de::Error::custom(e.to_string()))?;

        Ok(manifest)
    }
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
        let manifest: Self = serde_json::from_str(json).map_err(|e| {
            let msg = e.to_string();

            let found_opt = msg
                .starts_with("unsupported manifest version ")
                .then(|| msg.split_whitespace().nth(3))
                .flatten()
                .and_then(|s| s.replace(',', "").parse::<u32>().ok());

            if let Some(found) = found_opt {
                return Error::UnsupportedManifestVersion {
                    found,
                    supported: 1,
                };
            }

            let path_err_opt = msg
                .starts_with("invalid path in VM ")
                .then(|| msg.split_whitespace().nth(4))
                .flatten()
                .map(|s| s.replace(':', ""))
                .and_then(|vm| msg.find(&format!("{vm}: ")).map(|p| (vm, p)));

            if let Some((vm, path_start)) = path_err_opt {
                let path_str = &msg[path_start + vm.len() + 2..];
                let path_str = path_str.split(" (must be").next().unwrap_or(path_str);
                return Error::InvalidPath {
                    vm,
                    path: std::path::PathBuf::from(path_str),
                };
            }

            Error::Json(e)
        })?;
        Ok(manifest)
    }

    /// Serializes the [`SnapshotManifest`] to a compact JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if any VM file path is absolute or contains parent components,
    /// or [`Error::Json`] if serialization fails.
    pub fn to_json_string(&self) -> Result<String, Error> {
        if self.format_version != 1 {
            return Err(Error::UnsupportedManifestVersion {
                found: self.format_version,
                supported: 1,
            });
        }
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
        if self.format_version != 1 {
            return Err(Error::UnsupportedManifestVersion {
                found: self.format_version,
                supported: 1,
            });
        }
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
        self.validate_paths()?;

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
                    let n = match reader.read(&mut buffer) {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            return Err(Error::Io {
                                vm: vm_name.clone(),
                                path: file.path.clone(),
                                source: e,
                            });
                        }
                    };
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

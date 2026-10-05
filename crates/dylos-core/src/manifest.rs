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

const FORMAT_VERSION: u32 = 1;

/// Reads only `format_version`, so an unknown version is reported as such even when the
/// rest of the document follows another schema. Other fields are ignored here, but a
/// duplicated `format_version` is still rejected.
#[derive(Deserialize)]
struct VersionProbe {
    format_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSnapshotManifest {
    format_version: u32,
    lab_spec: LabSpec,
    #[serde(deserialize_with = "unique_keys")]
    vms: BTreeMap<String, VmManifest>,
    firecracker_version: String,
    host_cpu_model: String,
    created_at_unix_ms: u64,
    step_durations_ms: StepDurations,
}

/// A JSON object with a repeated VM name must be rejected: a plain `BTreeMap` would keep
/// the last entry silently.
fn unique_keys<'de, D>(deserializer: D) -> Result<BTreeMap<String, VmManifest>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UniqueKeys;

    impl<'de> serde::de::Visitor<'de> for UniqueKeys {
        type Value = BTreeMap<String, VmManifest>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of VM names to VM manifests")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((name, vm)) = map.next_entry::<String, VmManifest>()? {
                if out.contains_key(&name) {
                    return Err(serde::de::Error::custom(format!("duplicate VM `{name}`")));
                }
                out.insert(name, vm);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_map(UniqueKeys)
}

impl RawSnapshotManifest {
    fn into_checked(self) -> Result<SnapshotManifest, Error> {
        if self.format_version != FORMAT_VERSION {
            return Err(Error::UnsupportedManifestVersion {
                found: self.format_version,
                supported: FORMAT_VERSION,
            });
        }
        let manifest = SnapshotManifest {
            format_version: self.format_version,
            lab_spec: self.lab_spec,
            vms: self.vms,
            firecracker_version: self.firecracker_version,
            host_cpu_model: self.host_cpu_model,
            created_at_unix_ms: self.created_at_unix_ms,
            step_durations_ms: self.step_durations_ms,
        };
        manifest.validate_paths()?;
        Ok(manifest)
    }
}

/// Direct serde deserialization applies the same version and path checks, but in a single
/// pass: an unknown version with an incompatible schema surfaces as a schema error. Use
/// [`SnapshotManifest::from_json_str`] to get the typed errors.
impl<'de> Deserialize<'de> for SnapshotManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        RawSnapshotManifest::deserialize(deserializer)?
            .into_checked()
            .map_err(serde::de::Error::custom)
    }
}

impl SnapshotManifest {
    /// Deserializes and validates a [`SnapshotManifest`] from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedManifestVersion`] if `format_version` is not 1 (checked
    /// before the rest of the schema), [`Error::Json`] if the JSON is malformed or does not
    /// match the schema (including duplicate fields or VM names), or [`Error::InvalidPath`]
    /// if any VM file path is absolute or contains parent components.
    pub fn from_json_str(json: &str) -> Result<Self, Error> {
        let probe: VersionProbe = serde_json::from_str(json)?;
        if probe.format_version != FORMAT_VERSION {
            return Err(Error::UnsupportedManifestVersion {
                found: probe.format_version,
                supported: FORMAT_VERSION,
            });
        }
        let raw: RawSnapshotManifest = serde_json::from_str(json)?;
        raw.into_checked()
    }

    /// Serializes the [`SnapshotManifest`] to a compact JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] if any VM file path is absolute or contains parent components,
    /// or [`Error::Json`] if serialization fails.
    pub fn to_json_string(&self) -> Result<String, Error> {
        if self.format_version != FORMAT_VERSION {
            return Err(Error::UnsupportedManifestVersion {
                found: self.format_version,
                supported: FORMAT_VERSION,
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
        if self.format_version != FORMAT_VERSION {
            return Err(Error::UnsupportedManifestVersion {
                found: self.format_version,
                supported: FORMAT_VERSION,
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

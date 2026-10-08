use std::collections::HashMap;

use dylos_core::LabSpec;

use crate::Error;

/// Longest Linux interface name: `IFNAMSIZ` (16) minus the trailing NUL.
pub const IFNAME_MAX_LEN: usize = 15;

const LAB_ID_MAX_LEN: usize = 64;

/// Bridge of one `LabSpec` segment, named `br-<segment>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bridge {
    pub segment: String,
    pub name: String,
}

/// TAP of one VM interface, named `tap-<node>-<interface>` and enslaved to `bridge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tap {
    pub node: String,
    pub interface: String,
    pub name: String,
    pub bridge: String,
}

/// Every host-side name of a lab fabric, computed and validated before anything is created.
///
/// Bridge and TAP names depend only on the `LabSpec`, not on the lab id: they live inside the
/// lab's own netns, so every clone reuses them unchanged (ADR-0001, ADR-0002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricPlan {
    lab_id: String,
    bridges: Vec<Bridge>,
    taps: Vec<Tap>,
}

impl FabricPlan {
    /// # Errors
    ///
    /// Invalid lab id, unknown segment, or a derived interface name that is too long, uses
    /// characters other than ASCII letters, digits, `-` and `_`, or collides with another one.
    pub fn new(lab_id: &str, spec: &LabSpec) -> Result<Self, Error> {
        if lab_id.is_empty() || lab_id.len() > LAB_ID_MAX_LEN || !is_portable(lab_id) {
            return Err(Error::InvalidLabId(lab_id.to_owned()));
        }
        let mut seen: HashMap<String, String> = HashMap::new();
        let mut claim = |ifname: String, source_desc: String| -> Result<String, Error> {
            let reason = if ifname.len() > IFNAME_MAX_LEN {
                Some("is longer than 15 bytes (IFNAMSIZ)")
            } else if !is_portable(&ifname) {
                Some("may only contain ASCII letters, digits, '-' and '_'")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(Error::InvalidIfName {
                    source_desc,
                    ifname,
                    reason,
                });
            }
            if let Some(first) = seen.insert(ifname.clone(), source_desc.clone()) {
                return Err(Error::IfNameCollision {
                    ifname,
                    first,
                    second: source_desc,
                });
            }
            Ok(ifname)
        };

        let mut bridges = Vec::with_capacity(spec.segments.len());
        for segment in &spec.segments {
            let name = claim(
                format!("br-{}", segment.name),
                format!("segment {:?}", segment.name),
            )?;
            bridges.push(Bridge {
                segment: segment.name.clone(),
                name,
            });
        }
        let mut taps = Vec::new();
        for node in &spec.nodes {
            for iface in &node.interfaces {
                let bridge = bridges
                    .iter()
                    .find(|b| b.segment == iface.segment)
                    .ok_or_else(|| Error::UnknownSegment {
                        node: node.name.clone(),
                        interface: iface.name.clone(),
                        segment: iface.segment.clone(),
                    })?
                    .name
                    .clone();
                let name = claim(
                    format!("tap-{}-{}", node.name, iface.name),
                    format!("node {:?} interface {:?}", node.name, iface.name),
                )?;
                taps.push(Tap {
                    node: node.name.clone(),
                    interface: iface.name.clone(),
                    name,
                    bridge,
                });
            }
        }
        Ok(Self {
            lab_id: lab_id.to_owned(),
            bridges,
            taps,
        })
    }

    #[must_use]
    pub fn lab_id(&self) -> &str {
        &self.lab_id
    }

    /// File name of the lab's netns under the netns directory.
    #[must_use]
    pub fn netns_name(&self) -> String {
        format!("dylos-{}", self.lab_id)
    }

    #[must_use]
    pub fn bridges(&self) -> &[Bridge] {
        &self.bridges
    }

    #[must_use]
    pub fn taps(&self) -> &[Tap] {
        &self.taps
    }
}

fn is_portable(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

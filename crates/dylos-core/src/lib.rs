#![forbid(unsafe_code)]

pub mod error;
pub mod model;

pub use error::Error;
pub use model::{Interface, LabSpec, Node, Segment, StaticRoute};

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

impl LabSpec {
    #[allow(clippy::missing_errors_doc)]
    pub fn from_yaml_str(yaml: &str) -> Result<Self, Error> {
        let spec: LabSpec = serde_saphyr::from_str(yaml)?;
        spec.validate()?;
        Ok(spec)
    }

    #[allow(clippy::missing_errors_doc)]
    pub fn validate(&self) -> Result<(), Error> {
        let mut node_names = HashSet::new();
        let mut segment_names = HashMap::new();

        // 1. Unique segment names
        for (i, segment) in self.segments.iter().enumerate() {
            if segment_names.insert(&segment.name, &segment.cidr).is_some() {
                return Err(Error::DuplicateSegmentName {
                    path: format!("segments[{i}]"),
                    name: segment.name.clone(),
                });
            }
        }

        // 2. Unique node names
        for (i, node) in self.nodes.iter().enumerate() {
            if !node_names.insert(&node.name) {
                return Err(Error::DuplicateNodeName {
                    path: format!("nodes[{i}]"),
                    name: node.name.clone(),
                });
            }

            let mut iface_names = HashSet::new();
            let mut node_segments = HashSet::new();

            for (j, iface) in node.interfaces.iter().enumerate() {
                // Unique interface names (per node)
                if !iface_names.insert(&iface.name) {
                    return Err(Error::DuplicateInterfaceName {
                        path: format!("nodes[{i}].interfaces[{j}]"),
                        name: iface.name.clone(),
                    });
                }

                // Reference segment exists
                let Some(cidr) = segment_names.get(&iface.segment) else {
                    return Err(Error::UnknownSegment {
                        path: format!("nodes[{i}].interfaces[{j}]"),
                        segment: iface.segment.clone(),
                    });
                };

                // IP inside segment CIDR
                if !cidr.contains(&iface.ip.addr()) {
                    return Err(Error::IpOutsideSegment {
                        path: format!("nodes[{i}].interfaces[{j}].ip"),
                        ip: iface.ip.addr(),
                        segment: iface.segment.clone(),
                        cidr: **cidr,
                    });
                }

                node_segments.insert(iface.segment.clone());
            }

            for (j, route) in node.static_routes.iter().enumerate() {
                // Gateway reachable on one of the node's segments?
                // This means the gateway IP must be inside the CIDR of one of the segments the node is connected to.
                let mut reachable = false;
                for seg_name in &node_segments {
                    if let Some(cidr) = segment_names.get(seg_name)
                        && cidr.contains(&route.gateway)
                    {
                        reachable = true;
                        break;
                    }
                }

                if !reachable {
                    return Err(Error::UnreachableGateway {
                        path: format!("nodes[{i}].static_routes[{j}]"),
                        gateway: route.gateway,
                    });
                }
            }
        }

        // 3. No duplicate IP on a segment
        let mut segment_ips: HashMap<&String, HashSet<IpAddr>> = HashMap::new();
        for (i, node) in self.nodes.iter().enumerate() {
            for (j, iface) in node.interfaces.iter().enumerate() {
                let ips = segment_ips.entry(&iface.segment).or_default();
                if !ips.insert(iface.ip.addr()) {
                    return Err(Error::DuplicateIp {
                        path: format!("nodes[{i}].interfaces[{j}]"),
                        ip: iface.ip.addr(),
                    });
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod proptest_gen;

#[cfg(test)]
mod prop_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn test_round_trip_and_valid(spec in proptest_gen::valid_lab_spec()) {
            assert!(spec.validate().is_ok());
            let yaml = serde_saphyr::to_string(&spec).unwrap();
            let parsed: LabSpec = LabSpec::from_yaml_str(&yaml).expect("should parse");
            assert_eq!(spec, parsed);
        }
    }
}

#[cfg(test)]
mod manual_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_abc_yaml() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../labs/abc.yaml");
        let yaml = std::fs::read_to_string(&path).unwrap();
        let spec = LabSpec::from_yaml_str(&yaml).expect("should parse and validate abc.yaml");
        assert_eq!(spec.nodes.len(), 3);
        assert_eq!(spec.segments.len(), 2);
    }
}

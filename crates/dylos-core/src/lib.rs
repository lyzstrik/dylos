//! Pure domain definitions and logic for Dylos.
//!
//! # Current status of the spike
//!
//! The core models what exists for the technical spike.
//!
//! # Target Design Constraints
//!
//! This crate contains the structural definition of a lab (`LabSpec`), its validation,
//! and the snapshot manifest format.
//! It sits at the bottom of the dependency graph (see `docs/architecture.md`)
//! and must **never** depend on system-specific or async runtime crates
//! (like `tokio`, `nix`, `libc`, or `rtnetlink`).
#![forbid(unsafe_code)]

pub mod error;
pub mod guest_net;
pub mod mac;
pub mod manifest;
pub mod model;

pub use error::Error;
pub use mac::MacAddr;
pub use model::{Interface, LabSpec, Node, Segment, StaticRoute};

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

impl LabSpec {
    /// Parses a YAML string into a validated `LabSpec`.
    ///
    /// # Errors
    ///
    /// Returns an error if the YAML is structurally invalid (malformed or missing required fields)
    /// or if it fails semantic validation. See [`LabSpec::validate`] for examples of semantic errors.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, Error> {
        let spec: LabSpec = serde_saphyr::from_str(yaml)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Validates the semantic constraints of the lab spec.
    ///
    /// # Errors
    ///
    /// Returns an error if semantic constraints are violated. Examples include:
    /// - Invalid names (e.g., too long, illegal characters)
    /// - Duplicate segment or node names
    /// - Unknown segment references
    /// - Duplicate IP addresses on the same segment
    /// - IPs outside their segment's CIDR block
    /// - Unreachable static route gateways
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), Error> {
        let mut node_names = HashSet::new();
        let mut segment_names = HashMap::new();

        for (i, segment) in self.segments.iter().enumerate() {
            if let Err(reason) = validate_name(&segment.name, 32) {
                return Err(Error::InvalidName {
                    path: format!("segments[{i}].name"),
                    value: segment.name.clone(),
                    reason,
                });
            }
            if segment_names.insert(&segment.name, segment).is_some() {
                return Err(Error::DuplicateSegmentName {
                    path: format!("segments[{i}]"),
                    name: segment.name.clone(),
                });
            }
        }

        let mut segment_ips: HashMap<&String, HashSet<IpAddr>> = HashMap::new();

        for (i, node) in self.nodes.iter().enumerate() {
            if let Err(reason) = validate_name(&node.name, 32) {
                return Err(Error::InvalidName {
                    path: format!("nodes[{i}].name"),
                    value: node.name.clone(),
                    reason,
                });
            }
            if let Err(reason) = validate_name(&node.image, usize::MAX) {
                return Err(Error::InvalidName {
                    path: format!("nodes[{i}].image"),
                    value: node.image.clone(),
                    reason,
                });
            }
            if !node_names.insert(&node.name) {
                return Err(Error::DuplicateNodeName {
                    path: format!("nodes[{i}]"),
                    name: node.name.clone(),
                });
            }

            let mut iface_names = HashSet::new();
            let mut node_iface_prefixes = Vec::new();
            let mut node_own_addresses = HashSet::new();

            for (j, iface) in node.interfaces.iter().enumerate() {
                if let Err(reason) = validate_name(&iface.name, 15) {
                    return Err(Error::InvalidName {
                        path: format!("nodes[{i}].interfaces[{j}].name"),
                        value: iface.name.clone(),
                        reason,
                    });
                }
                if !iface_names.insert(&iface.name) {
                    return Err(Error::DuplicateInterfaceName {
                        path: format!("nodes[{i}].interfaces[{j}]"),
                        name: iface.name.clone(),
                    });
                }

                let Some(segment) = segment_names.get(&iface.segment) else {
                    return Err(Error::UnknownSegment {
                        path: format!("nodes[{i}].interfaces[{j}]"),
                        segment: iface.segment.clone(),
                    });
                };

                if !segment.ipv6.contains(&iface.ipv6.addr()) {
                    return Err(Error::IpOutsideSegment {
                        path: format!("nodes[{i}].interfaces[{j}].ipv6"),
                        ip: IpAddr::V6(iface.ipv6.addr()),
                        segment: iface.segment.clone(),
                        cidr: ipnet::IpNet::V6(segment.ipv6),
                    });
                }

                if iface.ipv6.prefix_len() < segment.ipv6.prefix_len() {
                    return Err(Error::PrefixWiderThanSegment {
                        path: format!("nodes[{i}].interfaces[{j}].ipv6"),
                        prefix_len: iface.ipv6.prefix_len(),
                        segment: iface.segment.clone(),
                        segment_prefix_len: segment.ipv6.prefix_len(),
                    });
                }

                let ips = segment_ips.entry(&iface.segment).or_default();
                if !ips.insert(IpAddr::V6(iface.ipv6.addr())) {
                    return Err(Error::DuplicateIp {
                        path: format!("nodes[{i}].interfaces[{j}]"),
                        ip: IpAddr::V6(iface.ipv6.addr()),
                    });
                }

                node_iface_prefixes.push(ipnet::IpNet::V6(iface.ipv6));
                node_own_addresses.insert(IpAddr::V6(iface.ipv6.addr()));

                match (segment.ipv4, iface.ipv4) {
                    (Some(seg_v4), Some(iface_v4)) => {
                        if !seg_v4.contains(&iface_v4.addr()) {
                            return Err(Error::IpOutsideSegment {
                                path: format!("nodes[{i}].interfaces[{j}].ipv4"),
                                ip: IpAddr::V4(iface_v4.addr()),
                                segment: iface.segment.clone(),
                                cidr: ipnet::IpNet::V4(seg_v4),
                            });
                        }
                        if iface_v4.prefix_len() < seg_v4.prefix_len() {
                            return Err(Error::PrefixWiderThanSegment {
                                path: format!("nodes[{i}].interfaces[{j}].ipv4"),
                                prefix_len: iface_v4.prefix_len(),
                                segment: iface.segment.clone(),
                                segment_prefix_len: seg_v4.prefix_len(),
                            });
                        }
                        if !ips.insert(IpAddr::V4(iface_v4.addr())) {
                            return Err(Error::DuplicateIp {
                                path: format!("nodes[{i}].interfaces[{j}]"),
                                ip: IpAddr::V4(iface_v4.addr()),
                            });
                        }
                        node_iface_prefixes.push(ipnet::IpNet::V4(iface_v4));
                        node_own_addresses.insert(IpAddr::V4(iface_v4.addr()));
                    }
                    (None, None) => {}
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(Error::Ipv4Mismatch {
                            path: format!("nodes[{i}].interfaces[{j}]"),
                            segment: iface.segment.clone(),
                        });
                    }
                }
            }

            for (j, route) in node.static_routes.iter().enumerate() {
                let dest_is_ipv4 = matches!(route.destination, ipnet::IpNet::V4(_));
                let gw_is_ipv4 = route.gateway.is_ipv4();

                if dest_is_ipv4 != gw_is_ipv4 {
                    return Err(Error::FamilyMismatch {
                        path: format!("nodes[{i}].static_routes[{j}]"),
                    });
                }

                if node_own_addresses.contains(&route.gateway) {
                    return Err(Error::UnreachableGateway {
                        path: format!("nodes[{i}].static_routes[{j}]"),
                        gateway: route.gateway,
                    });
                }

                let mut reachable = false;
                for prefix in &node_iface_prefixes {
                    if prefix.contains(&route.gateway) {
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

        Ok(())
    }
}

/// Validates a resource name against the safe-name rules.
///
/// # Errors
/// Returns a string slice describing the reason if the name is invalid.
pub fn validate_name(name: &str, max_len: usize) -> Result<(), &'static str> {
    if name.len() > max_len {
        return Err("too long");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("cannot be empty");
    };
    if !first.is_ascii_alphanumeric() {
        return Err("must start with an alphanumeric character");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("can only contain alphanumeric characters and '-'");
    }
    Ok(())
}

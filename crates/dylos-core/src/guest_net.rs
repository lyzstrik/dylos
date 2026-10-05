use crate::{Error, LabSpec, Node};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write;

/// The maximum size of the Dylos network parameters on the kernel command line.
/// (`x86_64` has a 4096-byte limit for the entire command line)
pub const BOOT_ARGS_BUDGET: usize = 2048;

/// Internal helper to generate a MAC from raw hash bytes, exposed for testing.
#[doc(hidden)]
#[must_use]
pub fn mac_from_hash_input(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let result = hasher.finalize();

    // 02 is locally administered unicast
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        result[0], result[1], result[2], result[3], result[4]
    )
}

/// Derives a deterministic, locally administered unicast MAC address.
#[must_use]
pub fn mac_for_interface(node_name: &str, iface_name: &str) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(node_name.as_bytes());
    input.push(b':');
    input.extend_from_slice(iface_name.as_bytes());
    mac_from_hash_input(&input)
}

/// Validates that MAC addresses are unique within each segment.
///
/// # Errors
/// Returns an error if two interfaces on the same segment generate the same MAC address.
pub fn validate_macs(lab: &LabSpec) -> Result<(), Error> {
    let mut segments: HashMap<&str, HashMap<String, (String, String)>> = HashMap::new();

    for node in &lab.nodes {
        for iface in &node.interfaces {
            let mac = mac_for_interface(&node.name, &iface.name);
            let macs = segments.entry(&iface.segment).or_default();

            if let Some(existing) = macs.get(&mac) {
                return Err(Error::MacCollision(Box::new(
                    crate::error::MacCollisionInfo {
                        segment: iface.segment.clone(),
                        node1: existing.0.clone(),
                        iface1: existing.1.clone(),
                        node2: node.name.clone(),
                        iface2: iface.name.clone(),
                        mac,
                    },
                )));
            }
            macs.insert(mac, (node.name.clone(), iface.name.clone()));
        }
    }

    Ok(())
}

/// Generates Dylos-specific network configuration kernel command line parameters.
/// Format: `dylos.if=<name>,<mac>,<ipv6/len>[,<ipv4/len>]` `dylos.rt=<dst/len>,<gw>` `dylos.fwd=1`
///
/// # Errors
/// Returns an error if the budget is exceeded or if there is a MAC address collision.
pub fn boot_args(lab: &LabSpec, node: &Node) -> Result<String, Error> {
    validate_macs(lab)?;

    let mut args = String::new();

    for iface in &node.interfaces {
        let mac = mac_for_interface(&node.name, &iface.name);

        let mut if_arg = format!(
            "dylos.if={},{},{}/{}",
            iface.name,
            mac,
            iface.ipv6.addr(),
            iface.ipv6.prefix_len()
        );
        if let Some(ipv4) = iface.ipv4 {
            write!(&mut if_arg, ",{}/{}", ipv4.addr(), ipv4.prefix_len())?;
        }

        if !args.is_empty() {
            args.push(' ');
        }
        args.push_str(&if_arg);
    }

    for route in &node.static_routes {
        if !args.is_empty() {
            args.push(' ');
        }
        write!(args, "dylos.rt={},{}", route.destination, route.gateway)?;
    }

    let is_router = node.interfaces.len() > 1;
    if is_router {
        if !args.is_empty() {
            args.push(' ');
        }
        args.push_str("dylos.fwd=1");
    }

    if args.len() > BOOT_ARGS_BUDGET {
        return Err(Error::BootArgsTooLong {
            node: node.name.clone(),
            budget: BOOT_ARGS_BUDGET,
            actual: args.len(),
        });
    }

    Ok(args)
}

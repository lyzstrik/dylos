use crate::MacAddr;
use crate::{Error, LabSpec, Node};
use std::collections::HashMap;
use std::fmt::Write;

/// The maximum size of the Dylos network parameters on the kernel command line.
/// (`x86_64` has a 4096-byte limit for the entire command line)
pub const BOOT_ARGS_BUDGET: usize = 2048;

/// Derives a deterministic, locally administered unicast MAC address.
///
/// Each name is length-prefixed before hashing: with a plain separator, node `a:b` with
/// interface `c` and node `a` with interface `b:c` would hash the same bytes.
#[must_use]
pub fn mac_for_interface(node_name: &str, iface_name: &str) -> MacAddr {
    let mut input = Vec::new();
    for name in [node_name, iface_name] {
        input.extend_from_slice(&(name.len() as u64).to_le_bytes());
        input.extend_from_slice(name.as_bytes());
    }
    MacAddr::from_seed(&input)
}

/// Validates that MAC addresses are unique within each segment.
///
/// # Errors
/// Returns an error if two interfaces on the same segment generate the same MAC address.
pub fn validate_macs(lab: &LabSpec) -> Result<(), Error> {
    let mut segments: HashMap<&str, HashMap<MacAddr, (String, String)>> = HashMap::new();

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
                        mac: mac.to_string(),
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

    if let Err(reason) = crate::validate_name(&node.name, 32) {
        return Err(Error::InvalidName {
            path: "node.name".to_string(),
            value: node.name.clone(),
            reason,
        });
    }

    for (j, iface) in node.interfaces.iter().enumerate() {
        if let Err(reason) = crate::validate_name(&iface.name, 15) {
            return Err(Error::InvalidName {
                path: format!("interfaces[{j}].name"),
                value: iface.name.clone(),
                reason,
            });
        }

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

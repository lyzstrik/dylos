use crate::{Error, LabSpec, Node};
use sha2::{Digest, Sha256};
use std::fmt::Write;

/// The maximum size of the Dylos network parameters on the kernel command line.
/// (`x86_64` has a 4096-byte limit for the entire command line)
pub const BOOT_ARGS_BUDGET: usize = 2048;

/// Derives a deterministic, locally administered unicast MAC address.
#[must_use]
pub fn mac_for_interface(node_name: &str, iface_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(node_name.as_bytes());
    hasher.update(b":");
    hasher.update(iface_name.as_bytes());
    let result = hasher.finalize();

    // 02 is locally administered unicast
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        result[0], result[1], result[2], result[3], result[4]
    )
}

/// Generates Dylos-specific network configuration kernel command line parameters.
/// Format: `dylos.if=<name>,<mac>,<ipv6/len>[,<ipv4/len>]` `dylos.rt=<dst/len>,<gw>` `dylos.fwd=1`
/// Returns an error if the budget is exceeded.
#[allow(clippy::missing_errors_doc)]
pub fn boot_args(_lab: &LabSpec, node: &Node) -> Result<String, Error> {
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
            let _ = write!(&mut if_arg, ",{}/{}", ipv4.addr(), ipv4.prefix_len());
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
        let _ = write!(args, "dylos.rt={},{}", route.destination, route.gateway);
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

use dylos_core::LabSpec;
use dylos_core::guest_net::{boot_args, mac_for_interface};
use serde_json::Value;
use std::process::Command;

#[test]
#[allow(clippy::too_many_lines)]
fn test_net_setup_integration() {
    let unshare_check = Command::new("unshare").args(["-Urn", "true"]).output();
    if unshare_check.is_err() || !unshare_check.unwrap().status.success() {
        println!("Skipping: unshare -Urn is not permitted on this host");
        return;
    }

    let yaml = std::fs::read_to_string("../../labs/abc.yaml").unwrap();
    let spec = LabSpec::from_yaml_str(&yaml).unwrap();
    let node_b = spec.nodes.iter().find(|n| n.name == "B").unwrap();
    let args = boot_args(&spec, node_b).unwrap();
    let mac0 = mac_for_interface("B", "eth0");
    let mac1 = mac_for_interface("B", "eth1");

    let temp_dir = tempfile::tempdir().unwrap();
    let cmdline_path = temp_dir.path().join("cmdline");
    std::fs::write(&cmdline_path, args).unwrap();

    let script_path = temp_dir.path().join("test_run.sh");
    let script = format!(
        r#"#!/bin/sh
set -e
mount -t sysfs none /sys
ip link add dummy0 type dummy
ip link set dummy0 address {mac0}
ip link add dummy1 type dummy
ip link set dummy1 address {mac1}

$(realpath ../../xtask/images/net-setup) {cmdline_file}

echo "===ADDR==="
ip -j addr show
echo "===ROUTE4==="
ip -j -4 route show
echo "===ROUTE6==="
ip -j -6 route show
echo "===SYSCTL==="
sysctl -n net.ipv6.conf.all.accept_dad
sysctl -n net.ipv6.conf.eth0.accept_dad
sysctl -n net.ipv6.conf.all.accept_ra
sysctl -n net.ipv6.conf.eth0.accept_ra
sysctl -n net.ipv6.conf.all.autoconf
sysctl -n net.ipv6.conf.eth0.autoconf
sysctl -n net.ipv6.conf.all.forwarding
sysctl -n net.ipv4.ip_forward
"#,
        cmdline_file = cmdline_path.display()
    );
    std::fs::write(&script_path, script).unwrap();
    Command::new("chmod")
        .args(["+x", script_path.to_str().unwrap()])
        .status()
        .unwrap();

    let output = Command::new("unshare")
        .args(["-Urnm", script_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "net-setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut sections = stdout.split("===");
    sections.next(); // empty before ADDR===
    let addr_json = sections.nth(1).unwrap().trim();
    let route4_json = sections.nth(1).unwrap().trim();
    let route6_json = sections.nth(1).unwrap().trim();
    let sysctls = sections.nth(1).unwrap().trim();

    let addrs: Value = serde_json::from_str(addr_json).unwrap();
    let route4: Value = serde_json::from_str(route4_json).unwrap();
    let route6: Value = serde_json::from_str(route6_json).unwrap();

    let mut has_v4_eth0 = false;
    let mut has_v6_eth0 = false;
    let mut has_v4_eth1 = false;
    let mut has_v6_eth1 = false;

    for iface in addrs.as_array().unwrap() {
        let name = iface["ifname"].as_str().unwrap();
        if let Some(addr_info) = iface["addr_info"].as_array() {
            for addr in addr_info {
                let local = addr["local"].as_str().unwrap();
                let prefixlen = addr["prefixlen"].as_u64().unwrap();
                if name == "eth0" && local == "10.0.1.1" && prefixlen == 24 {
                    has_v4_eth0 = true;
                }
                if name == "eth0" && local == "fd64:796c:6f73:1::1" && prefixlen == 64 {
                    has_v6_eth0 = true;
                }
                if name == "eth1" && local == "10.0.2.1" && prefixlen == 24 {
                    has_v4_eth1 = true;
                }
                if name == "eth1" && local == "fd64:796c:6f73:2::1" && prefixlen == 64 {
                    has_v6_eth1 = true;
                }
            }
        }
    }

    assert!(has_v4_eth0, "missing 10.0.1.1 on eth0");
    assert!(has_v6_eth0, "missing fd64:...:1::1 on eth0");
    assert!(has_v4_eth1, "missing 10.0.2.1 on eth1");
    assert!(has_v6_eth1, "missing fd64:...:2::1 on eth1");

    // Node B has no static routes, so we just ensure no routes with a gateway exist.
    for rt in route4.as_array().unwrap() {
        assert!(rt.get("gateway").is_none(), "unexpected gateway route");
    }
    for rt in route6.as_array().unwrap() {
        assert!(rt.get("gateway").is_none(), "unexpected gateway route");
    }

    let sysctl_lines: Vec<&str> = sysctls.lines().collect();
    assert_eq!(sysctl_lines.len(), 8);
    for (i, val) in sysctl_lines.iter().enumerate() {
        if i < 6 {
            assert_eq!(*val, "0", "sysctl index {i} should be 0");
        } else {
            assert_eq!(*val, "1", "sysctl index {i} should be 1");
        }
    }
}

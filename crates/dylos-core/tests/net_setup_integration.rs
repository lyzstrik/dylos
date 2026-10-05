use dylos_core::LabSpec;
use dylos_core::guest_net::{boot_args, mac_for_interface};
use serde_json::Value;
use std::error::Error;
use std::fmt::Write;
use std::process::Command;

fn run_net_setup(
    cmdline: &str,
    interfaces: &[(&str, &str)],
) -> Result<std::process::Output, Box<dyn Error>> {
    let temp_dir = tempfile::tempdir()?;
    let cmdline_path = temp_dir.path().join("cmdline");
    std::fs::write(&cmdline_path, cmdline)?;

    let mut script = String::from("#!/bin/sh\nset -e\nmount -t sysfs none /sys\n");
    for (i, (_ifname, mac)) in interfaces.iter().enumerate() {
        writeln!(&mut script, "ip link add dummy{i} type dummy")?;
        writeln!(&mut script, "ip link set dummy{i} address {mac}")?;
    }

    let net_setup_path = format!(
        "{}/../../xtask/images/net-setup",
        env!("CARGO_MANIFEST_DIR")
    );
    writeln!(
        &mut script,
        "$(realpath {net_setup_path}) {}",
        cmdline_path.display()
    )?;

    script.push_str(
        r#"
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
    );

    let script_path = temp_dir.path().join("test_run.sh");
    std::fs::write(&script_path, &script)?;
    Command::new("chmod")
        .args(["+x", script_path.to_str().ok_or("invalid path")?])
        .status()?;

    Ok(Command::new("unshare")
        .args(["-Urnm", script_path.to_str().ok_or("invalid path")?])
        .output()?)
}

fn can_unshare() -> bool {
    let unshare_check = Command::new("unshare").args(["-Urn", "true"]).output();
    unshare_check.is_ok()
        && unshare_check
            .unwrap_or_else(|_| unreachable!())
            .status
            .success()
}

fn get_spec() -> Result<LabSpec, Box<dyn Error>> {
    let yaml = std::fs::read_to_string(format!(
        "{}/../../labs/abc.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    Ok(LabSpec::from_yaml_str(&yaml)?)
}

fn check_no_other_globals(
    addrs: &Value,
    ifname: &str,
    allowed_globals: &[&str],
) -> Result<(), Box<dyn Error>> {
    for iface in addrs.as_array().ok_or("not array")? {
        if iface["ifname"].as_str().ok_or("no ifname")? == ifname
            && let Some(addr_info) = iface["addr_info"].as_array()
        {
            let mut has_link_local = false;
            for addr in addr_info {
                let scope = addr["scope"].as_str().unwrap_or("");
                let local = addr["local"].as_str().ok_or("no local")?;
                if scope == "link" && local.starts_with("fe80:") {
                    has_link_local = true;
                } else if scope == "global" {
                    assert!(
                        allowed_globals.contains(&local),
                        "unexpected global address: {local}"
                    );
                }
            }
            assert!(
                has_link_local,
                "missing link-local fe80:: address on {ifname}"
            );
        }
    }
    Ok(())
}

#[test]
fn test_net_setup_node_b() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    let spec = get_spec()?;
    let node_b = spec
        .nodes
        .iter()
        .find(|n| n.name == "B")
        .ok_or("node not found")?;
    let args = boot_args(&spec, node_b)?;
    let mac0 = mac_for_interface("B", "eth0");
    let mac1 = mac_for_interface("B", "eth1");

    let output = run_net_setup(&args, &[("eth0", &mac0), ("eth1", &mac1)])?;
    assert!(
        output.status.success(),
        "net-setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout)?;
    let mut sections = stdout.split("===");
    sections.next();
    sections.next();
    let addrs: Value = serde_json::from_str(sections.next().ok_or("no addrs")?.trim())?;
    sections.next();
    let route4: Value = serde_json::from_str(sections.next().ok_or("no route4")?.trim())?;
    sections.next();
    let route6: Value = serde_json::from_str(sections.next().ok_or("no route6")?.trim())?;
    sections.next();
    let sysctls = sections.next().ok_or("no sysctls")?.trim();

    check_no_other_globals(&addrs, "eth0", &["10.0.1.1", "fd64:796c:6f73:1::1"])?;
    check_no_other_globals(&addrs, "eth1", &["10.0.2.1", "fd64:796c:6f73:2::1"])?;

    for rt in route4.as_array().ok_or("route4 not array")? {
        assert!(rt.get("gateway").is_none(), "unexpected gateway route");
    }
    for rt in route6.as_array().ok_or("route6 not array")? {
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
    Ok(())
}

#[test]
fn test_net_setup_node_a() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    let spec = get_spec()?;
    let node_a = spec
        .nodes
        .iter()
        .find(|n| n.name == "A")
        .ok_or("not found")?;
    let args = boot_args(&spec, node_a)?;
    let mac0 = mac_for_interface("A", "eth0");

    let output = run_net_setup(&args, &[("eth0", &mac0)])?;
    assert!(
        output.status.success(),
        "net-setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout)?;
    let mut sections = stdout.split("===");
    sections.next();
    sections.next();
    let addrs: Value = serde_json::from_str(sections.next().ok_or("no addrs")?.trim())?;
    sections.next();
    let _route4 = sections.next().ok_or("no route4")?;
    sections.next();
    let _route6 = sections.next().ok_or("no route6")?;
    sections.next();
    let sysctls = sections.next().ok_or("no sysctls")?.trim();

    check_no_other_globals(&addrs, "eth0", &["10.0.1.2", "fd64:796c:6f73:1::2"])?;

    let sysctl_lines: Vec<&str> = sysctls.lines().collect();
    assert_eq!(sysctl_lines.len(), 8);
    for (i, val) in sysctl_lines.iter().enumerate() {
        assert_eq!(*val, "0", "sysctl index {i} should be 0");
    }
    Ok(())
}

#[test]
fn test_net_setup_node_c() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    let spec = get_spec()?;
    let node_c = spec
        .nodes
        .iter()
        .find(|n| n.name == "C")
        .ok_or("not found")?;
    let args = boot_args(&spec, node_c)?;
    let mac0 = mac_for_interface("C", "eth0");

    let output = run_net_setup(&args, &[("eth0", &mac0)])?;
    assert!(
        output.status.success(),
        "net-setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout)?;
    let mut sections = stdout.split("===");
    sections.next();
    sections.next();
    let addrs: Value = serde_json::from_str(sections.next().ok_or("no addrs")?.trim())?;
    sections.next();
    let _route4 = sections.next().ok_or("no route4")?;
    sections.next();
    let _route6 = sections.next().ok_or("no route6")?;
    sections.next();
    let sysctls = sections.next().ok_or("no sysctls")?.trim();

    check_no_other_globals(&addrs, "eth0", &["10.0.2.2", "fd64:796c:6f73:2::2"])?;

    let sysctl_lines: Vec<&str> = sysctls.lines().collect();
    for (i, val) in sysctl_lines.iter().enumerate() {
        assert_eq!(*val, "0", "sysctl index {i} should be 0");
    }
    Ok(())
}

#[test]
fn test_malformed_param() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }

    // Test missing fields
    let args = "dylos.if=eth0,02:00:00:00:00:01 dylos.fwd=1";
    let output = run_net_setup(args, &[("eth0", "02:00:00:00:00:01")])?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("malformed dylos.if parameter"),
        "missing IPv6 should be rejected"
    );

    // Test unknown parameter
    let args = "dylos.unknown=1";
    let output = run_net_setup(args, &[])?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown parameter"),
        "unknown parameter should be rejected"
    );

    Ok(())
}

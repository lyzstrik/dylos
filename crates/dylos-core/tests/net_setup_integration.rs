use dylos_core::LabSpec;
use dylos_core::guest_net::{boot_args, mac_for_interface};
use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::process::Command;

fn run_net_setup(
    cmdline: &str,
    interfaces: &[(&str, &str)],
) -> Result<std::process::Output, Box<dyn Error>> {
    let temp_dir = tempfile::tempdir()?;
    let cmdline_path = temp_dir.path().join("cmdline");
    std::fs::write(&cmdline_path, cmdline)?;

    let script_path = format!(
        "{}/tests/fixtures/net_setup_run.sh",
        env!("CARGO_MANIFEST_DIR")
    );
    let net_setup_path = format!(
        "{}/../../xtask/images/net-setup",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut command = Command::new("unshare");
    command
        .args(["-Urnm", "sh", &script_path, &net_setup_path])
        .arg(&cmdline_path);
    for (name, mac) in interfaces {
        command.args([name, mac]);
    }
    Ok(command.output()?)
}

fn can_unshare() -> bool {
    let ok = Command::new("unshare")
        .args(["-Urn", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("SKIPPED: unprivileged user+net namespaces unavailable");
    }
    ok
}

fn get_spec() -> Result<LabSpec, Box<dyn Error>> {
    let yaml = std::fs::read_to_string(format!(
        "{}/../../labs/abc.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    Ok(LabSpec::from_yaml_str(&yaml)?)
}

struct Observed {
    addrs: Value,
    route4: Value,
    route6: Value,
    sysctls: Vec<String>,
}

fn run_node(
    node: &str,
    initial_names: &[&str],
    swap_macs: bool,
) -> Result<Observed, Box<dyn Error>> {
    let spec = get_spec()?;
    let n = spec
        .nodes
        .iter()
        .find(|n| n.name == node)
        .ok_or("node not found")?;
    let args = boot_args(&spec, n)?;
    let declared: Vec<String> = n.interfaces.iter().map(|i| i.name.clone()).collect();
    let macs: Vec<String> = declared
        .iter()
        .map(|i| mac_for_interface(node, i).to_string())
        .collect();
    let mut interfaces: Vec<(&str, &str)> = initial_names
        .iter()
        .copied()
        .zip(macs.iter().map(String::as_str))
        .collect();
    if swap_macs {
        let (a, b) = (interfaces[0].1, interfaces[1].1);
        interfaces[0].1 = b;
        interfaces[1].1 = a;
    }

    let output = run_net_setup(&args, &interfaces)?;
    assert!(
        output.status.success(),
        "net-setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let mut sections = stdout.split("===");
    sections.next();
    sections.next();
    let addrs = serde_json::from_str(sections.next().ok_or("no addrs")?.trim())?;
    sections.next();
    let route4 = serde_json::from_str(sections.next().ok_or("no route4")?.trim())?;
    sections.next();
    let route6 = serde_json::from_str(sections.next().ok_or("no route6")?.trim())?;
    sections.next();
    let sysctls = sections
        .next()
        .ok_or("no sysctls")?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Observed {
        addrs,
        route4,
        route6,
        sysctls,
    })
}

fn assert_addrs(addrs: &Value, ifname: &str, expected: &[&str]) -> Result<(), Box<dyn Error>> {
    let iface = addrs
        .as_array()
        .ok_or("not array")?
        .iter()
        .find(|i| i["ifname"].as_str() == Some(ifname))
        .ok_or_else(|| format!("interface {ifname} is missing"))?;
    let mut globals = BTreeSet::new();
    let mut has_link_local = false;
    for addr in iface["addr_info"].as_array().ok_or("no addr_info")? {
        let local = addr["local"].as_str().ok_or("no local")?;
        let prefix = addr["prefixlen"].as_u64().ok_or("no prefixlen")?;
        if addr["scope"].as_str() == Some("global") {
            globals.insert(format!("{local}/{prefix}"));
        } else if local.starts_with("fe80:") {
            has_link_local = true;
        }
    }
    let expected: BTreeSet<String> = expected.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(globals, expected, "global addresses on {ifname}");
    assert!(has_link_local, "missing link-local address on {ifname}");
    Ok(())
}

fn gateway_routes(routes: &Value) -> Result<BTreeSet<(String, String)>, Box<dyn Error>> {
    let mut out = BTreeSet::new();
    for rt in routes.as_array().ok_or("routes not array")? {
        if let Some(gw) = rt["gateway"].as_str() {
            out.insert((
                rt["dst"].as_str().ok_or("no dst")?.to_owned(),
                gw.to_owned(),
            ));
        }
    }
    Ok(out)
}

fn assert_routes(
    obs: &Observed,
    v6: Option<(&str, &str)>,
    v4: Option<(&str, &str)>,
) -> Result<(), Box<dyn Error>> {
    let to_set = |r: Option<(&str, &str)>| -> BTreeSet<(String, String)> {
        r.map(|(d, g)| (d.to_owned(), g.to_owned()))
            .into_iter()
            .collect()
    };
    assert_eq!(
        gateway_routes(&obs.route6)?,
        to_set(v6),
        "IPv6 gateway routes"
    );
    assert_eq!(
        gateway_routes(&obs.route4)?,
        to_set(v4),
        "IPv4 gateway routes"
    );
    Ok(())
}

fn assert_sysctls(obs: &Observed, forwarding: bool) {
    assert_eq!(obs.sysctls.len(), 8);
    for (i, val) in obs.sysctls.iter().enumerate() {
        let want = if i >= 6 && forwarding { "1" } else { "0" };
        assert_eq!(val, want, "sysctl index {i}");
    }
}

fn check_node_b(obs: &Observed) -> Result<(), Box<dyn Error>> {
    assert_addrs(
        &obs.addrs,
        "eth0",
        &["10.0.1.1/24", "fd64:796c:6f73:1::1/64"],
    )?;
    assert_addrs(
        &obs.addrs,
        "eth1",
        &["10.0.2.1/24", "fd64:796c:6f73:2::1/64"],
    )?;
    assert_routes(obs, None, None)?;
    assert_sysctls(obs, true);
    Ok(())
}

#[test]
fn test_net_setup_node_b() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    check_node_b(&run_node("B", &["dummy0", "dummy1"], false)?)
}

#[test]
fn test_net_setup_swapped_macs_on_declared_names() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    // eth0 carries eth1's MAC and vice versa: a direct rename would hit "File exists".
    check_node_b(&run_node("B", &["eth0", "eth1"], true)?)
}

#[test]
fn test_net_setup_declared_name_equal_to_a_parking_name() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    // `dytmp1` is a valid declared name: parking names must avoid it, or the final
    // rename of the first NIC hits "File exists".
    let cmdline = "dylos.if=dytmp1,02:00:00:00:00:01,fd00:1::1/64,10.0.1.1/24 \
                   dylos.if=eth0,02:00:00:00:00:02,fd00:2::1/64,10.0.2.1/24";
    let output = run_net_setup(
        cmdline,
        &[("eth0", "02:00:00:00:00:01"), ("eth1", "02:00:00:00:00:02")],
    )?;
    assert!(
        output.status.success(),
        "net-setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let addrs_json = stdout
        .split("===ADDR===")
        .nth(1)
        .and_then(|s| s.split("===").next())
        .ok_or("no addrs")?;
    let addrs: Value = serde_json::from_str(addrs_json.trim())?;
    assert_addrs(&addrs, "dytmp1", &["fd00:1::1/64", "10.0.1.1/24"])?;
    assert_addrs(&addrs, "eth0", &["fd00:2::1/64", "10.0.2.1/24"])?;
    Ok(())
}

#[test]
fn test_net_setup_node_a() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    let obs = run_node("A", &["dummy0"], false)?;
    assert_addrs(
        &obs.addrs,
        "eth0",
        &["10.0.1.2/24", "fd64:796c:6f73:1::2/64"],
    )?;
    assert_routes(
        &obs,
        Some(("fd64:796c:6f73:2::/64", "fd64:796c:6f73:1::1")),
        Some(("10.0.2.0/24", "10.0.1.1")),
    )?;
    assert_sysctls(&obs, false);
    Ok(())
}

#[test]
fn test_net_setup_node_c() -> Result<(), Box<dyn Error>> {
    if !can_unshare() {
        return Ok(());
    }
    let obs = run_node("C", &["dummy0"], false)?;
    assert_addrs(
        &obs.addrs,
        "eth0",
        &["10.0.2.2/24", "fd64:796c:6f73:2::2/64"],
    )?;
    assert_routes(
        &obs,
        Some(("fd64:796c:6f73:1::/64", "fd64:796c:6f73:2::1")),
        Some(("10.0.1.0/24", "10.0.2.1")),
    )?;
    assert_sysctls(&obs, false);
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

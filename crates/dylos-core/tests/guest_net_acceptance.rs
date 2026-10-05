//! Acceptance tests for LYZ-16 (guest IP configuration at boot), derived from the issue,
//! the PR contract and ADR-0002 rather than from the implementation.

use dylos_core::guest_net::{BOOT_ARGS_BUDGET, boot_args, mac_for_interface, validate_macs};
use dylos_core::{Error, LabSpec, Node, StaticRoute};
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::fmt::Write;
use std::net::Ipv6Addr;
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn StdError>>;

// Two node names whose eth0 MACs collide (found by brute force over the 40 hash bits).
const COLLIDING_A: &str = "n34737";
const COLLIDING_B: &str = "n405021";

fn abc() -> Result<LabSpec, Box<dyn StdError>> {
    let yaml = std::fs::read_to_string(format!(
        "{}/../../labs/abc.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    Ok(LabSpec::from_yaml_str(&yaml)?)
}

fn node<'a>(lab: &'a LabSpec, name: &str) -> Result<&'a Node, Box<dyn StdError>> {
    lab.nodes
        .iter()
        .find(|n| n.name == name)
        .ok_or_else(|| format!("node {name} missing").into())
}

fn if_args(args: &str) -> Vec<&str> {
    args.split(' ')
        .filter_map(|a| a.strip_prefix("dylos.if="))
        .collect()
}

fn rt_args(args: &str) -> Vec<&str> {
    args.split(' ')
        .filter_map(|a| a.strip_prefix("dylos.rt="))
        .collect()
}

fn collision_lab(seg_a: &str, seg_b: &str) -> Result<LabSpec, Box<dyn StdError>> {
    let ip = |seg: &str, host: u8| format!("fd00:{}::{host}/64", &seg[1..]);
    let (ip_a, ip_b) = (ip(seg_a, 1), ip(seg_b, 2));
    let yaml = include_str!("fixtures/collision_lab.yaml")
        .replace("NODE_A", COLLIDING_A)
        .replace("NODE_B", COLLIDING_B)
        .replace("SEG_A", seg_a)
        .replace("SEG_B", seg_b)
        .replace("IP_A", &ip_a)
        .replace("IP_B", &ip_b);
    Ok(LabSpec::from_yaml_str(&yaml)?)
}

#[test]
fn mac_is_locally_administered_unicast_lowercase_and_well_formed() {
    for (n, i) in [
        ("A", "eth0"),
        ("B", "eth1"),
        ("node-with-long-name", "wan0"),
    ] {
        let mac = mac_for_interface(n, i).to_string();
        let octets: Vec<&str> = mac.split(':').collect();
        assert_eq!(octets.len(), 6, "{mac}");
        assert!(
            octets.iter().all(|o| o.len() == 2
                && o.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))),
            "{mac}"
        );
        let first = u8::from_str_radix(octets[0], 16).unwrap_or(0xff);
        assert_eq!(first & 0x01, 0, "multicast bit set in {mac}");
        assert_eq!(first & 0x02, 0x02, "not locally administered: {mac}");
    }
}

#[test]
fn mac_depends_on_node_and_interface_roles_and_boundaries() {
    assert_ne!(
        mac_for_interface("A", "B").to_string(),
        mac_for_interface("B", "A").to_string()
    );
    // A plain "<node>:<iface>" concatenation maps both pairs to the same string.
    assert_ne!(
        mac_for_interface("a:b", "c").to_string(),
        mac_for_interface("a", "b:c").to_string(),
        "node/interface boundary is ambiguous"
    );
    assert_ne!(
        mac_for_interface("ab", "c0").to_string(),
        mac_for_interface("abc", "0").to_string()
    );
}

#[test]
fn macs_of_abc_are_distinct_and_independent_of_lab_content() -> TestResult {
    let lab = abc()?;
    let mut seen = HashSet::new();
    for n in &lab.nodes {
        for i in &n.interfaces {
            assert!(seen.insert(mac_for_interface(&n.name, &i.name).to_string()));
        }
    }
    assert_eq!(seen.len(), 4);

    let mut reordered = lab.clone();
    reordered.nodes.reverse();
    for n in &reordered.nodes {
        let args = boot_args(&reordered, n)?;
        for i in &n.interfaces {
            assert!(args.contains(&mac_for_interface(&n.name, &i.name).to_string()));
        }
        assert_eq!(args, boot_args(&lab, node(&lab, &n.name)?)?);
    }
    Ok(())
}

#[test]
fn boot_args_are_deterministic() -> TestResult {
    let lab = abc()?;
    for n in &lab.nodes {
        assert_eq!(boot_args(&lab, n)?, boot_args(&lab, n)?);
    }
    Ok(())
}

#[test]
fn boot_args_exact_for_node_a() -> TestResult {
    let lab = abc()?;
    let mac = mac_for_interface("A", "eth0").to_string();
    assert_eq!(
        boot_args(&lab, node(&lab, "A")?)?,
        format!(
            "dylos.if=eth0,{mac},fd64:796c:6f73:1::2/64,10.0.1.2/24 \
             dylos.rt=fd64:796c:6f73:2::/64,fd64:796c:6f73:1::1 \
             dylos.rt=10.0.2.0/24,10.0.1.1"
        )
    );
    Ok(())
}

#[test]
fn boot_args_exact_for_router_b_keeps_interface_order() -> TestResult {
    let lab = abc()?;
    let m0 = mac_for_interface("B", "eth0").to_string();
    let m1 = mac_for_interface("B", "eth1").to_string();
    assert_eq!(
        boot_args(&lab, node(&lab, "B")?)?,
        format!(
            "dylos.if=eth0,{m0},fd64:796c:6f73:1::1/64,10.0.1.1/24 \
             dylos.if=eth1,{m1},fd64:796c:6f73:2::1/64,10.0.2.1/24 \
             dylos.fwd=1"
        )
    );
    Ok(())
}

#[test]
fn forwarding_only_for_multi_homed_nodes() -> TestResult {
    let lab = abc()?;
    for (name, expected) in [("A", 0), ("B", 1), ("C", 0)] {
        let args = boot_args(&lab, node(&lab, name)?)?;
        assert_eq!(
            args.split(' ').filter(|a| *a == "dylos.fwd=1").count(),
            expected,
            "{name}: {args}"
        );
    }
    let mut lone = node(&lab, "A")?.clone();
    lone.interfaces.clear();
    lone.static_routes.clear();
    assert_eq!(boot_args(&lab, &lone)?, "");
    Ok(())
}

#[test]
fn ipv6_only_interface_has_no_ipv4_field() -> TestResult {
    let mut lab = abc()?;
    for s in &mut lab.segments {
        s.ipv4 = None;
    }
    for n in &mut lab.nodes {
        n.static_routes.retain(|r| r.destination.addr().is_ipv6());
        for i in &mut n.interfaces {
            i.ipv4 = None;
        }
    }
    let args = boot_args(&lab, node(&lab, "A")?)?;
    let ifs = if_args(&args);
    assert_eq!(ifs.len(), 1);
    assert_eq!(ifs[0].split(',').count(), 3, "{args}");
    assert!(!args.contains("10.0."), "{args}");
    assert_eq!(rt_args(&args).len(), 1);
    Ok(())
}

#[test]
fn dual_stack_interface_lists_ipv6_before_ipv4() -> TestResult {
    let lab = abc()?;
    let args = boot_args(&lab, node(&lab, "C")?)?;
    let fields: Vec<&str> = if_args(&args)[0].split(',').collect();
    assert_eq!(fields.len(), 4);
    assert!(fields[2].parse::<ipnet::Ipv6Net>().is_ok(), "{fields:?}");
    assert!(fields[3].parse::<ipnet::Ipv4Net>().is_ok(), "{fields:?}");
    Ok(())
}

#[test]
fn routes_are_emitted_in_declared_order_per_family() -> TestResult {
    let lab = abc()?;
    let mut n = node(&lab, "A")?.clone();
    n.static_routes = vec![
        StaticRoute {
            destination: "10.9.0.0/16".parse()?,
            gateway: "10.0.1.1".parse()?,
        },
        StaticRoute {
            destination: "fd00:9::/48".parse()?,
            gateway: "fd64:796c:6f73:1::1".parse()?,
        },
        StaticRoute {
            destination: "10.8.0.0/16".parse()?,
            gateway: "10.0.1.1".parse()?,
        },
    ];
    let args = boot_args(&lab, &n)?;
    assert_eq!(
        rt_args(&args),
        [
            "10.9.0.0/16,10.0.1.1",
            "fd00:9::/48,fd64:796c:6f73:1::1",
            "10.8.0.0/16,10.0.1.1"
        ]
    );
    Ok(())
}

fn route_variant(base: &Node, dst: &str) -> Result<Node, Box<dyn StdError>> {
    let mut n = base.clone();
    n.static_routes.push(StaticRoute {
        destination: dst.parse()?,
        gateway: "10.0.1.1".parse()?,
    });
    Ok(n)
}

fn args_len(lab: &LabSpec, n: &Node) -> Result<usize, Box<dyn StdError>> {
    match boot_args(lab, n) {
        Ok(a) => Ok(a.len()),
        Err(Error::BootArgsTooLong { actual, .. }) => Ok(actual),
        Err(e) => Err(e.into()),
    }
}

// Pads node A with routes until its command line is exactly `target` bytes.
fn node_with_len(lab: &LabSpec, target: usize) -> Result<Node, Box<dyn StdError>> {
    let mut n = node(lab, "A")?.clone();
    let mut i = 1u32;
    while args_len(lab, &n)? + 60 < target {
        n = route_variant(&n, &format!("172.16.{}.{}/32", i / 250, i % 250))?;
        i += 1;
    }
    let cur = args_len(lab, &n)?;
    for a in 1..=255u32 {
        for b in 0..=255u32 {
            for mask in [8, 16, 24, 32] {
                let cand = route_variant(&n, &format!("{a}.{b}.0.0/{mask}"))?;
                if args_len(lab, &cand)? == target && cur < target {
                    return Ok(cand);
                }
            }
        }
        if a > 20 {
            break;
        }
    }
    Err("could not reach target length".into())
}

#[test]
fn boot_args_budget_is_inclusive_and_error_reports_sizes() -> TestResult {
    let lab = abc()?;
    let at = node_with_len(&lab, BOOT_ARGS_BUDGET)?;
    assert_eq!(boot_args(&lab, &at)?.len(), BOOT_ARGS_BUDGET);

    let over = node_with_len(&lab, BOOT_ARGS_BUDGET + 1)?;
    match boot_args(&lab, &over) {
        Err(Error::BootArgsTooLong {
            node,
            budget,
            actual,
        }) => {
            assert_eq!(node, "A");
            assert_eq!(budget, BOOT_ARGS_BUDGET);
            assert_eq!(actual, BOOT_ARGS_BUDGET + 1);
        }
        other => return Err(format!("expected BootArgsTooLong, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn mac_collision_in_one_segment_is_rejected_by_validate_and_boot_args() -> TestResult {
    assert_eq!(
        mac_for_interface(COLLIDING_A, "eth0").to_string(),
        mac_for_interface(COLLIDING_B, "eth0").to_string()
    );
    let lab = collision_lab("s1", "s1")?;
    match validate_macs(&lab) {
        Err(Error::MacCollision(info)) => assert_eq!(info.segment, "s1"),
        other => return Err(format!("expected MacCollision, got {other:?}").into()),
    }
    for n in &lab.nodes {
        assert!(matches!(boot_args(&lab, n), Err(Error::MacCollision(_))));
    }
    Ok(())
}

#[test]
fn equal_macs_on_different_segments_are_allowed() -> TestResult {
    let lab = collision_lab("s1", "s2")?;
    validate_macs(&lab)?;
    for n in &lab.nodes {
        boot_args(&lab, n)?;
    }
    Ok(())
}

#[test]
fn abc_passes_mac_validation() -> TestResult {
    validate_macs(&abc()?)?;
    Ok(())
}

// ---- net-setup guest script, exercised in a throwaway user+net namespace ----

struct Run {
    out: Output,
    sections: Vec<String>,
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

fn run_net_setup(cmdline: &str, macs: &[&str], sysctls: &[&str]) -> Result<Run, Box<dyn StdError>> {
    let dir = tempfile::tempdir()?;
    let cmdline_path = dir.path().join("cmdline");
    std::fs::write(&cmdline_path, cmdline)?;

    let mut script = String::from("#!/bin/sh\nset -e\nmount -t sysfs none /sys\n");
    for (i, mac) in macs.iter().enumerate() {
        writeln!(script, "ip link add dummy{i} type dummy")?;
        writeln!(script, "ip link set dummy{i} address {mac}")?;
    }
    let net_setup = std::fs::canonicalize(format!(
        "{}/../../xtask/images/net-setup",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    writeln!(
        script,
        "rc=0; sh {} {} || rc=$?",
        net_setup.display(),
        cmdline_path.display()
    )?;
    script.push_str("echo ===RC===; echo $rc\necho ===ADDR===; ip -j addr show\n");
    script.push_str("echo ===R4===; ip -j -4 route show\necho ===R6===; ip -j -6 route show\n");
    script.push_str("echo ===SYS===\n");
    for s in sysctls {
        writeln!(script, "echo {s}=$(sysctl -n {s})")?;
    }
    let script_path = dir.path().join("run.sh");
    std::fs::write(&script_path, script)?;

    let out = Command::new("unshare")
        .args(["-Urnm", "sh"])
        .arg(&script_path)
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let sections = stdout.split("===").skip(1).map(str::to_owned).collect();
    Ok(Run { out, sections })
}

impl Run {
    fn section(&self, name: &str) -> Result<&str, Box<dyn StdError>> {
        let idx = self
            .sections
            .iter()
            .position(|s| s == name)
            .ok_or_else(|| {
                format!(
                    "section {name} missing; stderr: {}",
                    String::from_utf8_lossy(&self.out.stderr)
                )
            })?;
        Ok(self.sections.get(idx + 1).map_or("", |s| s.trim()))
    }

    fn rc(&self) -> Result<i32, Box<dyn StdError>> {
        Ok(self.section("RC")?.parse()?)
    }

    fn json(&self, name: &str) -> Result<Value, Box<dyn StdError>> {
        Ok(serde_json::from_str(self.section(name)?)?)
    }

    fn sysctl(&self, key: &str) -> Result<String, Box<dyn StdError>> {
        self.section("SYS")?
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(str::to_owned)
            .ok_or_else(|| format!("no sysctl {key}").into())
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.out.stderr).into_owned()
    }
}

fn iface<'a>(addrs: &'a Value, name: &str) -> Result<&'a Value, Box<dyn StdError>> {
    addrs
        .as_array()
        .and_then(|a| a.iter().find(|i| i["ifname"] == name))
        .ok_or_else(|| format!("interface {name} missing").into())
}

fn infos(i: &Value) -> Vec<&Value> {
    i["addr_info"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn eui64_link_local(mac: &str) -> Result<Ipv6Addr, Box<dyn StdError>> {
    let o: Vec<u8> = mac
        .split(':')
        .map(|x| u8::from_str_radix(x, 16))
        .collect::<Result<_, _>>()?;
    Ok(Ipv6Addr::from([
        0xfe,
        0x80,
        0,
        0,
        0,
        0,
        0,
        0,
        o[0] ^ 0x02,
        o[1],
        o[2],
        0xff,
        0xfe,
        o[3],
        o[4],
        o[5],
    ]))
}

const SYSCTL_KEYS: [&str; 6] = [
    "net.ipv6.conf.eth0.accept_dad",
    "net.ipv6.conf.eth0.accept_ra",
    "net.ipv6.conf.eth0.autoconf",
    "net.ipv6.conf.eth1.accept_dad",
    "net.ipv6.conf.eth1.accept_ra",
    "net.ipv6.conf.eth1.autoconf",
];

#[test]
fn script_configures_each_abc_node_with_exactly_the_declared_addresses() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let lab = abc()?;
    for n in &lab.nodes {
        let macs: Vec<String> = n
            .interfaces
            .iter()
            .map(|i| mac_for_interface(&n.name, &i.name).to_string())
            .collect();
        let mac_refs: Vec<&str> = macs.iter().map(String::as_str).collect();
        let run = run_net_setup(&boot_args(&lab, n)?, &mac_refs, &[])?;
        assert_eq!(run.rc()?, 0, "{}: {}", n.name, run.stderr());
        let addrs = run.json("ADDR")?;
        for (i, want) in n.interfaces.iter().enumerate() {
            let got = iface(&addrs, &want.name)?;
            assert_eq!(got["address"], macs[i].as_str());
            assert!(
                got["flags"]
                    .as_array()
                    .is_some_and(|f| f.iter().any(|x| x == "UP"))
            );

            let mut globals: Vec<String> = infos(got)
                .iter()
                .filter(|a| a["scope"] == "global")
                .map(|a| format!("{}/{}", a["local"].as_str().unwrap_or(""), a["prefixlen"]))
                .collect();
            globals.sort();
            let mut expected = vec![format!("{}/{}", want.ipv6.addr(), want.ipv6.prefix_len())];
            if let Some(v4) = want.ipv4 {
                expected.push(format!("{}/{}", v4.addr(), v4.prefix_len()));
            }
            expected.sort();
            assert_eq!(globals, expected, "{}:{}", n.name, want.name);
        }
    }
    Ok(())
}

#[test]
fn script_matches_interfaces_by_mac_not_by_enumeration_order() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let lab = abc()?;
    let b = node(&lab, "B")?;
    let m0 = mac_for_interface("B", "eth0").to_string();
    let m1 = mac_for_interface("B", "eth1").to_string();
    // dummy0 carries eth1's MAC, so an implementation assigning by index swaps the networks.
    let run = run_net_setup(&boot_args(&lab, b)?, &[&m1, &m0], &[])?;
    assert_eq!(run.rc()?, 0, "{}", run.stderr());
    let addrs = run.json("ADDR")?;
    for (name, mac, v6) in [
        ("eth0", &m0, "fd64:796c:6f73:1::1"),
        ("eth1", &m1, "fd64:796c:6f73:2::1"),
    ] {
        let i = iface(&addrs, name)?;
        assert_eq!(i["address"], mac.as_str());
        assert!(infos(i).iter().any(|a| a["local"] == v6));
    }
    Ok(())
}

#[test]
fn script_static_ipv6_has_no_dad_and_link_local_is_eui64() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let lab = abc()?;
    let b = node(&lab, "B")?;
    let m0 = mac_for_interface("B", "eth0").to_string();
    let m1 = mac_for_interface("B", "eth1").to_string();
    let run = run_net_setup(&boot_args(&lab, b)?, &[&m0, &m1], &SYSCTL_KEYS)?;
    assert_eq!(run.rc()?, 0, "{}", run.stderr());
    let addrs = run.json("ADDR")?;
    for (name, mac) in [("eth0", &m0), ("eth1", &m1)] {
        let i = iface(&addrs, name)?;
        let all = infos(i);
        let ll: Vec<Ipv6Addr> = all
            .iter()
            .filter(|a| a["family"] == "inet6" && a["scope"] == "link")
            .filter_map(|a| a["local"].as_str()?.parse().ok())
            .collect();
        assert_eq!(ll, [eui64_link_local(mac)?], "{name}");
        for a in all.iter().filter(|a| a["family"] == "inet6") {
            assert_ne!(a["tentative"], true, "{name}: address left in DAD: {a}");
            assert_ne!(a["dadfailed"], true);
        }
    }
    for k in SYSCTL_KEYS {
        assert_eq!(run.sysctl(k)?, "0", "{k}");
    }
    Ok(())
}

#[test]
fn script_installs_exactly_the_declared_gateway_routes_per_family() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let lab = abc()?;
    for (name, dst6, gw6, dst4, gw4) in [
        (
            "A",
            "fd64:796c:6f73:2::/64",
            "fd64:796c:6f73:1::1",
            "10.0.2.0/24",
            "10.0.1.1",
        ),
        (
            "C",
            "fd64:796c:6f73:1::/64",
            "fd64:796c:6f73:2::1",
            "10.0.1.0/24",
            "10.0.2.1",
        ),
    ] {
        let n = node(&lab, name)?;
        let mac = mac_for_interface(name, "eth0").to_string();
        let run = run_net_setup(&boot_args(&lab, n)?, &[&mac], &[])?;
        assert_eq!(run.rc()?, 0, "{name}: {}", run.stderr());
        for (sec, dst, gw) in [("R6", dst6, gw6), ("R4", dst4, gw4)] {
            let routes = run.json(sec)?;
            let via: Vec<&Value> = routes
                .as_array()
                .ok_or("not an array")?
                .iter()
                .filter(|r| r.get("gateway").is_some())
                .collect();
            assert_eq!(via.len(), 1, "{name} {sec}: {routes}");
            assert_eq!(via[0]["dst"], dst);
            assert_eq!(via[0]["gateway"], gw);
            assert_eq!(via[0]["dev"], "eth0");
        }
    }
    Ok(())
}

#[test]
fn script_forwarding_follows_the_fwd_flag_for_both_families() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let lab = abc()?;
    let keys = ["net.ipv6.conf.all.forwarding", "net.ipv4.ip_forward"];
    for (name, want) in [("A", "0"), ("B", "1"), ("C", "0")] {
        let n = node(&lab, name)?;
        let macs: Vec<String> = n
            .interfaces
            .iter()
            .map(|i| mac_for_interface(name, &i.name).to_string())
            .collect();
        let refs: Vec<&str> = macs.iter().map(String::as_str).collect();
        let run = run_net_setup(&boot_args(&lab, n)?, &refs, &keys)?;
        assert_eq!(run.rc()?, 0, "{}", run.stderr());
        for k in keys {
            assert_eq!(run.sysctl(k)?, want, "{name} {k}");
        }
    }
    Ok(())
}

#[test]
fn script_handles_ipv6_only_interface_without_touching_ipv4() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let mac = "02:aa:bb:cc:dd:01";
    let cmdline = format!("dylos.if=eth0,{mac},fd00:1::5/64 dylos.rt=fd00:2::/64,fd00:1::1");
    let run = run_net_setup(&cmdline, &[mac], &[])?;
    assert_eq!(run.rc()?, 0, "{}", run.stderr());
    let addrs = run.json("ADDR")?;
    let i = iface(&addrs, "eth0")?;
    assert!(infos(i).iter().all(|a| a["family"] != "inet"));
    assert!(infos(i).iter().any(|a| a["local"] == "fd00:1::5"));
    Ok(())
}

#[test]
fn script_ignores_foreign_arguments_and_trailing_newline() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let lab = abc()?;
    let a = node(&lab, "A")?;
    let mac = mac_for_interface("A", "eth0").to_string();
    let cmdline = format!(
        "console=ttyS0 reboot=k panic=1 root=/dev/vda {} quiet\n",
        boot_args(&lab, a)?
    );
    let run = run_net_setup(&cmdline, &[&mac], &[])?;
    assert_eq!(run.rc()?, 0, "{}", run.stderr());
    assert!(
        infos(iface(&run.json("ADDR")?, "eth0")?)
            .iter()
            .any(|x| x["local"] == "10.0.1.2")
    );
    Ok(())
}

#[test]
fn script_fails_when_no_interface_has_the_declared_mac() -> TestResult {
    if !can_unshare() {
        return Ok(());
    }
    let cmdline = "dylos.if=eth0,02:00:00:00:00:99,fd00:1::5/64,10.0.0.5/24";
    let run = run_net_setup(cmdline, &["02:00:00:00:00:01"], &[])?;
    assert_ne!(run.rc()?, 0);
    assert!(
        run.stderr().contains("02:00:00:00:00:99"),
        "{}",
        run.stderr()
    );
    Ok(())
}

#[test]
fn script_fails_on_missing_cmdline_file() -> TestResult {
    let script = std::fs::canonicalize(format!(
        "{}/../../xtask/images/net-setup",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    let out = Command::new("sh")
        .arg(script)
        .arg("/nonexistent/dylos-cmdline")
        .output()?;
    assert!(!out.status.success());
    Ok(())
}

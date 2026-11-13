#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::process::{Command, Stdio};

use dylos_core::LabSpec;
use dylos_net::FabricPlan;

fn spec() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("fixtures/peer-filter.yaml")).unwrap()
}

#[test]
fn typed_policy_is_deterministic_and_uses_exact_guest_addresses() {
    let lab = spec();
    let a = FabricPlan::new("original", &lab).unwrap();
    let b = FabricPlan::new("clone", &lab).unwrap();
    let rules = a.peer_filter_rules().unwrap();
    assert_eq!(Some(rules.clone()), b.peer_filter_rules());
    assert!(rules.contains("hook prerouting"));
    assert!(rules.contains("iifname \"tap-a-eth0\" jump tap0"));
    assert!(rules.contains("ip6 saddr != { fd00::1, fe80::"));
    assert!(rules.contains("ip saddr != 192.0.2.1"));
    assert!(!rules.contains("/64"));
    assert!(!rules.contains("/24"));
    assert!(rules.contains("arp saddr ip 192.0.2.1"));
    assert!(rules.contains(
        "icmpv6 type 136 icmpv6 code 0 @th,64,128 { 0xfd000000000000000000000000000001, 0xfe80"
    ));
    assert!(rules.contains("ip6 length 32 @th,192,16 0x0201 @th,208,48"));
    assert!(rules.contains("ip6 nexthdr != { tcp, udp, ipv6-icmp } counter drop"));
    assert!(rules.contains("ip frag-off & 0x3fff != 0 counter drop"));
    assert!(rules.contains("udp sport 547 counter drop"));
    assert!(rules.contains("udp sport 67 counter drop"));
}

#[test]
fn disabled_segments_have_no_rules_and_ipv6_only_has_no_ipv4_exception() {
    let mut lab = spec();
    lab.peer_filtering = false;
    assert!(
        FabricPlan::new("lab", &lab)
            .unwrap()
            .peer_filter_rules()
            .is_none()
    );
    lab.segments[0].peer_filtering = Some(true);
    for node in &mut lab.nodes {
        node.interfaces[0].ipv4 = None;
    }
    let rules = FabricPlan::new("lab", &lab)
        .unwrap()
        .peer_filter_rules()
        .unwrap();
    assert!(rules.contains("tap-a-eth0"));
    assert!(!rules.contains("arp saddr"));
    assert!(!rules.contains("ip saddr"));
    lab.segments[0].peer_filtering = Some(false);
    lab.peer_filtering = true;
    assert!(
        FabricPlan::new("lab", &lab)
            .unwrap()
            .peer_filter_rules()
            .is_none()
    );
}

#[test]
fn kernel_accepts_rendered_atomic_batch_in_private_namespace() {
    if !Command::new("unshare")
        .args(["-Urmn", "true"])
        .status()
        .unwrap()
        .success()
    {
        eprintln!("SKIPPED nft renderer: unprivileged user namespaces unavailable");
        return;
    }
    let rules = FabricPlan::new("lab", &spec())
        .unwrap()
        .peer_filter_rules()
        .unwrap();
    let mut child = Command::new("unshare")
        .args(["-Urmn", "nft", "--check", "-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(rules.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn missing_policy_prepares_enforcement_by_default() {
    let yaml = include_str!("fixtures/peer-filter.yaml").replace("peer_filtering: true\n", "");
    let lab = LabSpec::from_yaml_str(&yaml).unwrap();
    assert!(lab.peer_filtering);
    assert!(
        FabricPlan::new("legacy", &lab)
            .unwrap()
            .peer_filter_rules()
            .is_some()
    );
}

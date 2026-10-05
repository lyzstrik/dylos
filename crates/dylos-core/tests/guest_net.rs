use dylos_core::LabSpec;
use dylos_core::guest_net::{BOOT_ARGS_BUDGET, boot_args, mac_for_interface};
use std::collections::HashSet;

#[test]
fn test_mac_determinism() {
    let mac1 = mac_for_interface("A", "eth0");
    let mac2 = mac_for_interface("A", "eth0");
    assert_eq!(mac1, mac2);
    assert_eq!(mac1.len(), 17);
    assert!(mac1.starts_with("02:"));

    let mac3 = mac_for_interface("B", "eth0");
    let mac4 = mac_for_interface("A", "eth1");
    assert_ne!(mac1, mac3);
    assert_ne!(mac1, mac4);
}

#[test]
fn test_mac_uniqueness_abc() {
    let yaml = std::fs::read_to_string("../../labs/abc.yaml").unwrap();
    let spec = LabSpec::from_yaml_str(&yaml).unwrap();

    let mut macs = HashSet::new();
    for node in &spec.nodes {
        for iface in &node.interfaces {
            let mac = mac_for_interface(&node.name, &iface.name);
            assert!(macs.insert(mac), "duplicate mac");
        }
    }
}

#[test]
fn test_boot_args_abc() {
    let yaml = std::fs::read_to_string("../../labs/abc.yaml").unwrap();
    let spec = LabSpec::from_yaml_str(&yaml).unwrap();

    let node_a = spec.nodes.iter().find(|n| n.name == "A").unwrap();
    let args_a = boot_args(&spec, node_a).unwrap();
    assert!(args_a.contains("dylos.if=eth0,"));
    assert!(args_a.contains("fd64:796c:6f73:1::2/64,10.0.1.2/24"));
    assert!(args_a.contains("dylos.rt=fd64:796c:6f73:2::/64,fd64:796c:6f73:1::1"));
    assert!(args_a.contains("dylos.rt=10.0.2.0/24,10.0.1.1"));
    assert!(!args_a.contains("dylos.fwd=1"));

    let node_b = spec.nodes.iter().find(|n| n.name == "B").unwrap();
    let args_b = boot_args(&spec, node_b).unwrap();
    assert!(args_b.contains("dylos.if=eth0,"));
    assert!(args_b.contains("dylos.if=eth1,"));
    assert!(args_b.contains("dylos.fwd=1"));

    let node_c = spec.nodes.iter().find(|n| n.name == "C").unwrap();
    let args_c = boot_args(&spec, node_c).unwrap();
    assert!(args_c.contains("dylos.if=eth0,"));
    assert!(args_c.contains("fd64:796c:6f73:2::2/64,10.0.2.2/24"));
    assert!(args_c.contains("dylos.rt=fd64:796c:6f73:1::/64,fd64:796c:6f73:2::1"));
    assert!(args_c.contains("dylos.rt=10.0.1.0/24,10.0.2.1"));
    assert!(!args_c.contains("dylos.fwd=1"));
}

#[test]
fn test_length_budget() {
    let yaml = std::fs::read_to_string("../../labs/abc.yaml").unwrap();
    let spec = LabSpec::from_yaml_str(&yaml).unwrap();
    for node in &spec.nodes {
        let args = boot_args(&spec, node).unwrap();
        assert!(args.len() < BOOT_ARGS_BUDGET);
    }
}

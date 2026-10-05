use dylos_core::LabSpec;
use dylos_core::guest_net::{BOOT_ARGS_BUDGET, boot_args, mac_for_interface};
use std::collections::HashSet;
use std::error::Error;

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
fn test_mac_uniqueness_abc() -> Result<(), Box<dyn Error>> {
    let yaml = std::fs::read_to_string(format!(
        "{}/../../labs/abc.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    let spec = LabSpec::from_yaml_str(&yaml)?;

    let mut macs = HashSet::new();
    for node in &spec.nodes {
        for iface in &node.interfaces {
            let mac = mac_for_interface(&node.name, &iface.name);
            assert!(macs.insert(mac), "duplicate mac");
        }
    }
    Ok(())
}

#[test]
fn test_boot_args_abc() -> Result<(), Box<dyn Error>> {
    let yaml = std::fs::read_to_string(format!(
        "{}/../../labs/abc.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    let spec = LabSpec::from_yaml_str(&yaml)?;

    let node_a = spec
        .nodes
        .iter()
        .find(|n| n.name == "A")
        .ok_or("node A not found")?;
    let args_a = boot_args(&spec, node_a)?;
    assert!(args_a.contains("dylos.if=eth0,"));
    assert!(args_a.contains("fd64:796c:6f73:1::2/64,10.0.1.2/24"));
    assert!(args_a.contains("dylos.rt=fd64:796c:6f73:2::/64,fd64:796c:6f73:1::1"));
    assert!(args_a.contains("dylos.rt=10.0.2.0/24,10.0.1.1"));
    assert!(!args_a.contains("dylos.fwd=1"));

    let node_b = spec
        .nodes
        .iter()
        .find(|n| n.name == "B")
        .ok_or("node B not found")?;
    let args_b = boot_args(&spec, node_b)?;
    assert!(args_b.contains("dylos.if=eth0,"));
    assert!(args_b.contains("dylos.if=eth1,"));
    assert!(args_b.contains("dylos.fwd=1"));

    let node_c = spec
        .nodes
        .iter()
        .find(|n| n.name == "C")
        .ok_or("node C not found")?;
    let args_c = boot_args(&spec, node_c)?;
    assert!(args_c.contains("dylos.if=eth0,"));
    assert!(args_c.contains("fd64:796c:6f73:2::2/64,10.0.2.2/24"));
    assert!(args_c.contains("dylos.rt=fd64:796c:6f73:1::/64,fd64:796c:6f73:2::1"));
    assert!(args_c.contains("dylos.rt=10.0.1.0/24,10.0.2.1"));
    assert!(!args_c.contains("dylos.fwd=1"));
    Ok(())
}

#[test]
fn test_length_budget() -> Result<(), Box<dyn Error>> {
    let yaml = std::fs::read_to_string(format!(
        "{}/../../labs/abc.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    let spec = LabSpec::from_yaml_str(&yaml)?;
    for node in &spec.nodes {
        let args = boot_args(&spec, node)?;
        assert!(args.len() < BOOT_ARGS_BUDGET);
    }
    Ok(())
}

#[test]
fn test_mac_collision_validation() -> Result<(), Box<dyn Error>> {
    // Found by brute force: these names collide on the 40 hash bits used for the MAC.
    let node1 = "n34737";
    let node2 = "n405021";

    let mac1 = dylos_core::guest_net::mac_for_interface(node1, "eth0");
    let mac2 = dylos_core::guest_net::mac_for_interface(node2, "eth0");
    assert_eq!(mac1, mac2);

    let yaml = format!(
        "
nodes:
  - name: {node1}
    image: dummy
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: test
        ipv6: fd00::1/64
  - name: {node2}
    image: dummy
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: test
        ipv6: fd00::2/64
segments:
  - name: test
    ipv6: fd00::/64
"
    );

    let spec = LabSpec::from_yaml_str(&yaml)?;
    let result = dylos_core::guest_net::validate_macs(&spec);
    assert!(result.is_err());
    if let Err(dylos_core::Error::MacCollision(info)) = result {
        assert_eq!(info.segment, "test");
        assert_eq!(info.mac, mac1);
        assert_eq!(info.iface1, "eth0");
        assert_eq!(info.iface2, "eth0");
        assert!(
            (info.node1 == node1 && info.node2 == node2)
                || (info.node1 == node2 && info.node2 == node1)
        );
    } else {
        return Err("expected MacCollision error".into());
    }
    Ok(())
}

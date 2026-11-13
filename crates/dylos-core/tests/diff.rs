#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use dylos_core::diff::TopologyDiff;
use dylos_core::{Interface, LabSpec, Node, Segment};

fn sample_spec() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("fixtures/diff-topology.yaml")).unwrap()
}

#[test]
fn topology_diff_identical_specs() {
    let spec = sample_spec();
    let diff = TopologyDiff::diff(&spec, &spec);
    assert!(diff.is_empty());
    assert_eq!(diff.segments.added, Vec::<String>::new());
    assert_eq!(diff.nodes.added, Vec::<String>::new());
    assert_eq!(diff.interfaces.added, Vec::new());
}

#[test]
fn topology_diff_segments() {
    let spec_a = sample_spec();
    let mut spec_b = sample_spec();
    spec_b.segments.retain(|s| s.name != "seg2");
    spec_b.segments.push(Segment {
        peer_filtering: None,
        name: "seg3".into(),
        ipv6: "2001:db8:3::/64".parse().unwrap(),
        ipv4: None,
    });
    spec_b.segments[0].ipv6 = "2001:db8:10::/64".parse().unwrap();

    let diff = TopologyDiff::diff(&spec_a, &spec_b);
    assert!(!diff.is_empty());
    assert_eq!(diff.segments.added, vec!["seg3"]);
    assert_eq!(diff.segments.removed, vec!["seg2"]);
    assert_eq!(diff.segments.changed.len(), 1);
    assert_eq!(diff.segments.changed[0].name, "seg1");
    assert_eq!(
        diff.segments.changed[0].ipv6.as_ref().unwrap().to,
        "2001:db8:10::/64".parse().unwrap()
    );
}

#[test]
fn topology_diff_nodes_and_interfaces() {
    let spec_a = sample_spec();
    let mut spec_b = sample_spec();

    spec_b.nodes[0].vcpus = 4;
    spec_b.nodes[0].memory = 512;

    spec_b.nodes[0].interfaces[0].ipv6 = "2001:db8:1::5/64".parse().unwrap();
    spec_b.nodes[0].interfaces.push(Interface {
        name: "eth1".into(),
        segment: "seg2".into(),
        ipv6: "2001:db8:2::2/64".parse().unwrap(),
        ipv4: None,
    });

    spec_b.nodes.push(Node {
        name: "vm2".into(),
        image: "alpine-3-18".into(),
        vcpus: 1,
        memory: 128,
        interfaces: vec![Interface {
            name: "eth0".into(),
            segment: "seg1".into(),
            ipv6: "2001:db8:1::3/64".parse().unwrap(),
            ipv4: None,
        }],
        static_routes: vec![],
    });

    let diff = TopologyDiff::diff(&spec_a, &spec_b);
    assert_eq!(diff.nodes.added, vec!["vm2"]);
    assert_eq!(diff.nodes.removed, Vec::<String>::new());
    assert_eq!(diff.nodes.changed.len(), 1);
    assert_eq!(diff.nodes.changed[0].name, "vm1");
    assert_eq!(diff.nodes.changed[0].vcpus.as_ref().unwrap().to, 4);
    assert_eq!(diff.nodes.changed[0].memory.as_ref().unwrap().to, 512);

    assert_eq!(diff.interfaces.added.len(), 2);
    assert_eq!(diff.interfaces.changed.len(), 1);
    assert_eq!(diff.interfaces.changed[0].node, "vm1");
    assert_eq!(diff.interfaces.changed[0].interface, "eth0");
}

#[test]
fn topology_diff_static_routes() {
    let spec_a = sample_spec();
    let mut spec_b = sample_spec();

    spec_b.nodes[0].static_routes = vec![dylos_core::StaticRoute {
        destination: "192.168.0.0/16".parse().unwrap(),
        gateway: "10.0.1.1".parse().unwrap(),
    }];

    let diff = TopologyDiff::diff(&spec_a, &spec_b);
    assert!(!diff.is_empty());
    assert_eq!(diff.nodes.changed.len(), 1);
    assert_eq!(diff.nodes.changed[0].name, "vm1");
    let routes = diff.nodes.changed[0].static_routes.as_ref().unwrap();
    assert_eq!(routes.from, Vec::<dylos_core::StaticRoute>::new());
    assert_eq!(routes.to.len(), 1);
}

#[test]
fn topology_diff_dual_stack_ipv4_changes() {
    let spec_a = sample_spec();
    let mut spec_b = sample_spec();

    spec_b.segments[0].ipv4 = Some("10.0.99.0/24".parse().unwrap());
    spec_b.nodes[0].interfaces[0].ipv4 = Some("10.0.99.2/24".parse().unwrap());

    let diff = TopologyDiff::diff(&spec_a, &spec_b);
    assert_eq!(diff.segments.changed.len(), 1);
    assert_eq!(
        diff.segments.changed[0].ipv4.as_ref().unwrap().to,
        Some("10.0.99.0/24".parse().unwrap())
    );
    assert_eq!(diff.interfaces.changed.len(), 1);
    assert_eq!(
        diff.interfaces.changed[0].ipv4.as_ref().unwrap().to,
        Some("10.0.99.2/24".parse().unwrap())
    );
}

#[test]
fn topology_diff_json_stability() {
    let spec = sample_spec();
    let diff = TopologyDiff::diff(&spec, &spec);
    let json = serde_json::to_string_pretty(&diff).unwrap();

    // Stably contains empty arrays and all container keys
    assert!(json.contains("\"added\": []"));
    assert!(json.contains("\"removed\": []"));
    assert!(json.contains("\"changed\": []"));
}

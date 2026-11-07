#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use dylos_core::diff::TopologyDiff;
use dylos_core::{Interface, LabSpec, Node, Segment};

fn sample_spec() -> LabSpec {
    LabSpec {
        segments: vec![
            Segment {
                name: "seg1".into(),
                ipv6: "2001:db8:1::/64".parse().unwrap(),
                ipv4: Some("10.0.1.0/24".parse().unwrap()),
            },
            Segment {
                name: "seg2".into(),
                ipv6: "2001:db8:2::/64".parse().unwrap(),
                ipv4: None,
            },
        ],
        nodes: vec![Node {
            name: "vm1".into(),
            image: "alpine-3-18".into(),
            vcpus: 2,
            memory: 256,
            interfaces: vec![Interface {
                name: "eth0".into(),
                segment: "seg1".into(),
                ipv6: "2001:db8:1::2/64".parse().unwrap(),
                ipv4: Some("10.0.1.2/24".parse().unwrap()),
            }],
            static_routes: vec![],
        }],
    }
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

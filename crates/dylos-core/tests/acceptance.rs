//! Acceptance tests for LYZ-14 (LabSpec topology model), derived from the
//! issue criteria and exercising only the public API.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty,
    clippy::doc_markdown
)]

use std::net::Ipv4Addr;
use std::path::PathBuf;

use dylos_core::{Interface, LabSpec, Node, Segment, StaticRoute};
use proptest::prelude::*;

const VALID: &str = "\
segments:
  - name: left
    cidr: 10.0.1.0/24
  - name: right
    cidr: 10.0.2.0/24
nodes:
  - name: a
    image: alpine
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: left
        ip: 10.0.1.2/24
    static_routes:
      - destination: 10.0.2.0/24
        gateway: 10.0.1.1
  - name: b
    image: alpine
    vcpus: 2
    memory: 512
    interfaces:
      - name: eth0
        segment: left
        ip: 10.0.1.1/24
      - name: eth1
        segment: right
        ip: 10.0.2.1/24
";

/// Parse `yaml` and return the error message, panicking if it is accepted.
fn err_of(yaml: &str) -> String {
    match LabSpec::from_yaml_str(yaml) {
        Ok(_) => panic!("expected rejection, but YAML was accepted:\n{yaml}"),
        Err(e) => e.to_string(),
    }
}

fn valid() -> LabSpec {
    LabSpec::from_yaml_str(VALID).expect("baseline must be valid")
}

fn validate_err(spec: &LabSpec) -> String {
    spec.validate()
        .expect_err("expected validation failure")
        .to_string()
}

// ---------------------------------------------------------------- 1. format

#[test]
fn yaml_format_parses_all_fields() {
    let spec = valid();
    assert_eq!(spec.segments.len(), 2);
    assert_eq!(spec.segments[0].name, "left");
    assert_eq!(spec.segments[0].cidr.to_string(), "10.0.1.0/24");
    assert_eq!(spec.nodes.len(), 2);
    let a = &spec.nodes[0];
    assert_eq!(a.name, "a");
    assert_eq!(a.image, "alpine");
    assert_eq!(a.vcpus, 1);
    assert_eq!(a.memory, 256);
    assert_eq!(a.interfaces[0].name, "eth0");
    assert_eq!(a.interfaces[0].segment, "left");
    assert_eq!(a.interfaces[0].ip.to_string(), "10.0.1.2/24");
    assert_eq!(a.static_routes[0].destination.to_string(), "10.0.2.0/24");
    assert_eq!(a.static_routes[0].gateway.to_string(), "10.0.1.1");
}

#[test]
fn yaml_node_without_interfaces_or_routes_is_valid() {
    let yaml = "\
segments: []
nodes:
  - name: lonely
    image: alpine
    vcpus: 1
    memory: 128
";
    let spec = LabSpec::from_yaml_str(yaml).expect("optional lists may be omitted");
    assert!(spec.nodes[0].interfaces.is_empty());
    assert!(spec.nodes[0].static_routes.is_empty());
}

#[test]
fn yaml_rejects_missing_required_node_fields() {
    // Each line below is the first node's; removing it must be rejected.
    for line in [
        "  - name: a\n    image: alpine\n",
        "    image: alpine\n    vcpus: 1\n",
        "    vcpus: 1\n    memory: 256\n",
        "    memory: 256\n    interfaces:\n      - name: eth0\n        segment: left\n        ip: 10.0.1.2/24\n",
    ] {
        let replacement = match line {
            l if l.starts_with("  - name: a") => "  - image: alpine\n",
            l if l.starts_with("    image") => "    vcpus: 1\n",
            l if l.starts_with("    vcpus") => "    memory: 256\n",
            _ => {
                "    interfaces:\n      - name: eth0\n        segment: left\n        ip: 10.0.1.2/24\n"
            }
        };
        let yaml = VALID.replacen(line, replacement, 1);
        assert_ne!(yaml, VALID, "mutation did not apply for {line:?}");
        err_of(&yaml);
    }
}

#[test]
fn yaml_rejects_missing_segment_cidr() {
    let yaml = "segments:\n  - name: s\nnodes: []\n";
    err_of(yaml);
}

#[test]
fn yaml_rejects_missing_interface_ip() {
    let yaml = VALID.replace("        ip: 10.0.1.2/24\n", "");
    err_of(&yaml);
}

#[test]
fn yaml_rejects_missing_top_level_sections() {
    err_of("segments: []\n");
    err_of("nodes: []\n");
}

fn rejects_unknown(from: &str, to: &str) {
    let yaml = VALID.replacen(from, to, 1);
    assert_ne!(yaml, VALID);
    err_of(&yaml);
}

#[test]
fn yaml_rejects_unknown_top_level_field() {
    rejects_unknown("segments:\n", "bogus: 1\nsegments:\n");
}

#[test]
fn yaml_rejects_unknown_node_field() {
    rejects_unknown("    vcpus: 1\n", "    vcpus: 1\n    colour: red\n");
}

#[test]
fn yaml_rejects_unknown_segment_field() {
    rejects_unknown(
        "    cidr: 10.0.1.0/24\n",
        "    cidr: 10.0.1.0/24\n    mtu: 1500\n",
    );
}

#[test]
fn yaml_rejects_unknown_interface_field() {
    rejects_unknown(
        "        ip: 10.0.1.2/24\n",
        "        ip: 10.0.1.2/24\n        mac: aa\n",
    );
}

#[test]
fn yaml_rejects_unknown_route_field() {
    rejects_unknown(
        "        gateway: 10.0.1.1\n",
        "        gateway: 10.0.1.1\n        metric: 5\n",
    );
}

#[test]
fn yaml_rejects_wrong_types() {
    err_of(&VALID.replace("vcpus: 1", "vcpus: many"));
    err_of(&VALID.replace("cidr: 10.0.1.0/24", "cidr: not-a-cidr"));
    err_of(&VALID.replace("gateway: 10.0.1.1", "gateway: nope"));
}

// ------------------------------------------------------------ 2. validation

#[test]
fn baseline_validates() {
    valid().validate().unwrap();
}

#[test]
fn rejects_duplicate_node_name() {
    let mut s = valid();
    s.nodes[1].name = s.nodes[0].name.clone();
    let m = validate_err(&s);
    assert!(m.contains("nodes[1]"), "{m}");
}

#[test]
fn rejects_duplicate_segment_name() {
    let mut s = valid();
    s.segments[1].name = s.segments[0].name.clone();
    let m = validate_err(&s);
    assert!(m.contains("segments[1]"), "{m}");
}

#[test]
fn rejects_duplicate_interface_name_on_node() {
    let mut s = valid();
    s.nodes[1].interfaces[1].name = "eth0".into();
    let m = validate_err(&s);
    assert!(m.contains("nodes[1].interfaces[1]"), "{m}");
}

#[test]
fn same_interface_name_on_different_nodes_is_fine() {
    // `eth0` already appears on both nodes in the baseline.
    valid().validate().unwrap();
}

#[test]
fn rejects_ip_outside_segment_cidr() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ip = "192.168.9.9/24".parse().unwrap();
    let m = validate_err(&s);
    assert!(m.contains("nodes[1].interfaces[0].ip"), "{m}");
}

#[test]
fn rejects_duplicate_ip_on_segment() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ip = "10.0.1.2/24".parse().unwrap(); // same as node a
    let m = validate_err(&s);
    assert!(m.contains("nodes[1].interfaces[0]"), "{m}");
}

#[test]
fn same_ip_on_different_segments_is_allowed() {
    let yaml = "\
segments:
  - name: s1
    cidr: 10.0.0.0/24
  - name: s2
    cidr: 10.0.0.0/24
nodes:
  - name: a
    image: i
    vcpus: 1
    memory: 64
    interfaces:
      - name: eth0
        segment: s1
        ip: 10.0.0.5/24
      - name: eth1
        segment: s2
        ip: 10.0.0.5/24
";
    LabSpec::from_yaml_str(yaml).expect("IP uniqueness is per segment");
}

#[test]
fn rejects_unknown_segment() {
    let mut s = valid();
    s.nodes[0].interfaces[0].segment = "ghost".into();
    let m = validate_err(&s);
    assert!(m.contains("nodes[0].interfaces[0]"), "{m}");
    assert!(m.contains("ghost"), "{m}");
}

#[test]
fn rejects_gateway_not_in_any_attached_segment() {
    let mut s = valid();
    s.nodes[0].static_routes[0].gateway = "172.16.0.1".parse().unwrap();
    let m = validate_err(&s);
    assert!(m.contains("nodes[0].static_routes[0]"), "{m}");
}

#[test]
fn rejects_gateway_on_segment_the_node_is_not_attached_to() {
    // Node a only sits on `left`; 10.0.2.1 lives on `right`.
    let mut s = valid();
    s.nodes[0].static_routes[0].gateway = "10.0.2.1".parse().unwrap();
    let m = validate_err(&s);
    assert!(m.contains("nodes[0].static_routes[0]"), "{m}");
}

#[test]
fn from_yaml_str_runs_validation() {
    err_of(&VALID.replace("ip: 10.0.1.2/24", "ip: 10.9.9.9/24"));
}

// ---------------------------------------------------------------- 3. errors

#[test]
fn error_path_for_ip_outside_segment_through_yaml() {
    let yaml = VALID.replace("        ip: 10.0.1.1/24\n", "        ip: 10.0.7.1/24\n");
    let m = err_of(&yaml);
    assert!(m.contains("nodes[1].interfaces[0].ip"), "{m}");
}

#[test]
fn error_messages_are_human_readable() {
    let mut s = valid();
    s.nodes[0].interfaces[0].segment = "ghost".into();
    let m = validate_err(&s);
    assert!(m.len() > 15 && m.contains(' '), "{m}");
}

#[test]
fn parse_errors_keep_line_and_column() {
    // Type error on line 7 (vcpus).
    let yaml = "\
segments: []
nodes:
  - name: a
    image: alpine
    memory: 64
    interfaces: []
    vcpus: lots
";
    let m = err_of(yaml);
    assert!(m.contains('7'), "line number missing: {m}");
    let lower = m.to_lowercase();
    assert!(lower.contains("line"), "no 'line' in: {m}");
    assert!(lower.contains("col"), "no column in: {m}");
}

#[test]
fn syntax_errors_keep_line_and_column() {
    let m = err_of("segments: [\nnodes: }\n");
    let lower = m.to_lowercase();
    assert!(lower.contains("line"), "{m}");
    assert!(lower.contains("col"), "{m}");
}

// ------------------------------------------------------------- 4. proptest

/// Build a valid topology: `segs` /24 segments, `nodes` nodes, node `n`
/// attached to segment `s` iff bit `s` of `mask[n]` is set.
fn build(segs: usize, masks: &[u8]) -> LabSpec {
    let segments = (0..segs)
        .map(|s| Segment {
            name: format!("seg{s}"),
            cidr: format!("10.0.{s}.0/24").parse().unwrap(),
        })
        .collect();
    let nodes = masks
        .iter()
        .enumerate()
        .map(|(n, mask)| {
            let interfaces: Vec<Interface> = (0..segs)
                .filter(|s| mask & (1 << s) != 0)
                .map(|s| Interface {
                    name: format!("eth{s}"),
                    segment: format!("seg{s}"),
                    ip: format!("10.0.{s}.{}/24", n + 1).parse().unwrap(),
                })
                .collect();
            let static_routes = interfaces
                .first()
                .map(|i| {
                    let net = i.ip.addr().to_string();
                    let third: Ipv4Addr = net.parse().unwrap();
                    let o = third.octets();
                    vec![StaticRoute {
                        destination: "172.20.0.0/16".parse().unwrap(),
                        gateway: Ipv4Addr::new(o[0], o[1], o[2], 254).into(),
                    }]
                })
                .unwrap_or_default();
            Node {
                name: format!("node{n}"),
                image: "alpine".into(),
                vcpus: 1 + (n as u32 % 4),
                memory: 128 * (1 + n as u32),
                interfaces,
                static_routes,
            }
        })
        .collect();
    LabSpec { nodes, segments }
}

fn valid_spec() -> impl Strategy<Value = LabSpec> {
    (1usize..=4).prop_flat_map(|segs| {
        prop::collection::vec(any::<u8>(), 1..=6).prop_map(move |m| build(segs, &m))
    })
}

proptest! {
    #[test]
    fn generated_topologies_validate_and_round_trip(spec in valid_spec()) {
        prop_assert!(spec.validate().is_ok());
        let yaml = serde_saphyr::to_string(&spec).unwrap();
        let back = LabSpec::from_yaml_str(&yaml).unwrap();
        prop_assert_eq!(&spec, &back);
        // Second hop is stable too.
        let yaml2 = serde_saphyr::to_string(&back).unwrap();
        prop_assert_eq!(LabSpec::from_yaml_str(&yaml2).unwrap(), spec);
    }

    #[test]
    fn mutation_duplicate_ip_on_segment_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let spots: Vec<(usize, usize)> = spec.nodes.iter().enumerate()
            .flat_map(|(n, node)| (0..node.interfaces.len()).map(move |i| (n, i)))
            .collect();
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        let victim = spec.nodes[n].interfaces[i].clone();
        let mut bad = spec.clone();
        bad.nodes.push(Node {
            name: "intruder".into(),
            image: "alpine".into(),
            vcpus: 1,
            memory: 64,
            interfaces: vec![Interface { name: "ethx".into(), ..victim }],
            static_routes: vec![],
        });
        prop_assert!(bad.validate().is_err());
        let yaml = serde_saphyr::to_string(&bad).unwrap();
        prop_assert!(LabSpec::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn mutation_ip_outside_segment_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let spots: Vec<(usize, usize)> = spec.nodes.iter().enumerate()
            .flat_map(|(n, node)| (0..node.interfaces.len()).map(move |i| (n, i)))
            .collect();
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        let mut bad = spec;
        bad.nodes[n].interfaces[i].ip = "192.168.200.1/24".parse().unwrap();
        prop_assert!(bad.validate().is_err());
    }

    #[test]
    fn mutation_unknown_segment_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let spots: Vec<(usize, usize)> = spec.nodes.iter().enumerate()
            .flat_map(|(n, node)| (0..node.interfaces.len()).map(move |i| (n, i)))
            .collect();
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        let mut bad = spec;
        bad.nodes[n].interfaces[i].segment = "does-not-exist".into();
        prop_assert!(bad.validate().is_err());
    }
}

// ------------------------------------------------------------- 5. abc.yaml

#[test]
fn labs_abc_yaml_loads_and_validates() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../labs/abc.yaml");
    let yaml = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let spec = LabSpec::from_yaml_str(&yaml).expect("abc.yaml must parse and validate");
    let names: Vec<_> = spec.nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["A", "B", "C"]);
    assert_eq!(spec.segments.len(), 2);
    // B is the router: attached to both segments.
    assert_eq!(spec.nodes[1].interfaces.len(), 2);
}

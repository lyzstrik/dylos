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

use std::path::PathBuf;

use dylos_core::{Error, Interface, LabSpec, Node, Segment, StaticRoute};
use proptest::prelude::*;

const VALID: &str = "\
segments:
  - name: left
    ipv6: fd64:796c:6f73:1::/64
    ipv4: 10.0.1.0/24
  - name: right
    ipv6: fd64:796c:6f73:2::/64
    ipv4: 10.0.2.0/24
nodes:
  - name: a
    image: alpine
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: left
        ipv6: fd64:796c:6f73:1::2/64
        ipv4: 10.0.1.2/24
    static_routes:
      - destination: fd64:796c:6f73:2::/64
        gateway: fd64:796c:6f73:1::1
      - destination: 10.0.2.0/24
        gateway: 10.0.1.1
  - name: b
    image: alpine
    vcpus: 2
    memory: 512
    interfaces:
      - name: eth0
        segment: left
        ipv6: fd64:796c:6f73:1::1/64
        ipv4: 10.0.1.1/24
      - name: eth1
        segment: right
        ipv6: fd64:796c:6f73:2::1/64
        ipv4: 10.0.2.1/24
";

/// Parse `yaml` and return the error, panicking if it is accepted.
fn err_of(yaml: &str) -> Error {
    match LabSpec::from_yaml_str(yaml) {
        Ok(_) => panic!("expected rejection, but YAML was accepted:\n{yaml}"),
        Err(e) => e,
    }
}

/// Like `err_of`, but the error must be a syntax/type/missing-field error
/// raised while parsing (not by semantic validation).
fn parse_err_of(yaml: &str) -> String {
    match err_of(yaml) {
        e @ Error::Parse(_) => e.to_string(),
        other => panic!("expected Error::Parse, got {other:?} for:\n{yaml}"),
    }
}

fn valid() -> LabSpec {
    LabSpec::from_yaml_str(VALID).expect("baseline must be valid")
}

fn validate_err(spec: &LabSpec) -> Error {
    spec.validate().expect_err("expected validation failure")
}

// ---------------------------------------------------------------- 1. format

#[test]
fn yaml_format_parses_all_fields() {
    let spec = valid();
    assert_eq!(spec.segments.len(), 2);
    assert_eq!(spec.segments[0].name, "left");
    assert_eq!(spec.segments[0].ipv6.to_string(), "fd64:796c:6f73:1::/64");
    assert_eq!(spec.segments[0].ipv4.unwrap().to_string(), "10.0.1.0/24");
    assert_eq!(spec.nodes.len(), 2);
    let a = &spec.nodes[0];
    assert_eq!(a.name, "a");
    assert_eq!(a.image, "alpine");
    assert_eq!(a.vcpus, 1);
    assert_eq!(a.memory, 256);
    assert_eq!(a.interfaces[0].name, "eth0");
    assert_eq!(a.interfaces[0].segment, "left");
    assert_eq!(a.interfaces[0].ipv6.to_string(), "fd64:796c:6f73:1::2/64");
    assert_eq!(a.interfaces[0].ipv4.unwrap().to_string(), "10.0.1.2/24");
    assert_eq!(
        a.static_routes[0].destination.to_string(),
        "fd64:796c:6f73:2::/64"
    );
    assert_eq!(
        a.static_routes[0].gateway.to_string(),
        "fd64:796c:6f73:1::1"
    );
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
    for line in [
        "  - name: a\n    image: alpine\n",
        "    image: alpine\n    vcpus: 1\n",
        "    vcpus: 1\n    memory: 256\n",
        "    memory: 256\n    interfaces:\n      - name: eth0\n        segment: left\n        ipv6: fd64:796c:6f73:1::2/64\n        ipv4: 10.0.1.2/24\n",
    ] {
        let replacement = match line {
            l if l.starts_with("  - name: a") => "  - image: alpine\n",
            l if l.starts_with("    image") => "    vcpus: 1\n",
            l if l.starts_with("    vcpus") => "    memory: 256\n",
            _ => {
                "    interfaces:\n      - name: eth0\n        segment: left\n        ipv6: fd64:796c:6f73:1::2/64\n        ipv4: 10.0.1.2/24\n"
            }
        };
        let yaml = VALID.replacen(line, replacement, 1);
        assert_ne!(yaml, VALID, "mutation did not apply for {line:?}");
        parse_err_of(&yaml);
    }
}

#[test]
fn yaml_rejects_missing_segment_ipv6() {
    let yaml = "segments:\n  - name: s\nnodes: []\n";
    parse_err_of(yaml);
}

#[test]
fn yaml_rejects_missing_interface_ipv6() {
    let yaml = VALID.replace("        ipv6: fd64:796c:6f73:1::2/64\n", "");
    parse_err_of(&yaml);
}

#[test]
fn yaml_rejects_missing_top_level_sections() {
    parse_err_of("segments: []\n");
    parse_err_of("nodes: []\n");
}

fn rejects_unknown(from: &str, to: &str) {
    let yaml = VALID.replacen(from, to, 1);
    assert_ne!(yaml, VALID);
    parse_err_of(&yaml);
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
        "    ipv6: fd64:796c:6f73:1::/64\n",
        "    ipv6: fd64:796c:6f73:1::/64\n    mtu: 1500\n",
    );
}

#[test]
fn yaml_rejects_unknown_interface_field() {
    rejects_unknown(
        "        ipv6: fd64:796c:6f73:1::2/64\n",
        "        ipv6: fd64:796c:6f73:1::2/64\n        mac: aa\n",
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
    parse_err_of(&VALID.replace("vcpus: 1", "vcpus: many"));
    parse_err_of(&VALID.replace("ipv4: 10.0.1.0/24", "ipv4: not-a-cidr"));
    parse_err_of(&VALID.replace("gateway: 10.0.1.1", "gateway: nope"));
}

// ------------------------------------------------------------ 2. validation

fn ip(s: &str) -> std::net::IpAddr {
    s.parse().unwrap()
}

fn iface(name: &str, segment: &str, v6: &str, v4: Option<&str>) -> Interface {
    Interface {
        name: name.into(),
        segment: segment.into(),
        ipv6: v6.parse().unwrap(),
        ipv4: v4.map(|a| a.parse().unwrap()),
    }
}

#[track_caller]
fn assert_outside(e: Error, want_path: &str, want_ip: &str, want_segment: &str, want_cidr: &str) {
    match e {
        Error::IpOutsideSegment {
            path,
            ip,
            segment,
            cidr,
        } => {
            assert_eq!(path, want_path);
            assert_eq!(ip, self::ip(want_ip));
            assert_eq!(segment, want_segment);
            assert_eq!(cidr.to_string(), want_cidr);
        }
        other => panic!("expected IpOutsideSegment, got {other:?}"),
    }
}

#[track_caller]
fn assert_duplicate_ip(e: Error, want_path: &str, want_ip: &str) {
    match e {
        Error::DuplicateIp { path, ip } => {
            assert_eq!(path, want_path);
            assert_eq!(ip, self::ip(want_ip));
        }
        other => panic!("expected DuplicateIp, got {other:?}"),
    }
}

#[track_caller]
fn assert_unreachable(e: Error, want_path: &str, want_gw: &str) {
    match e {
        Error::UnreachableGateway { path, gateway } => {
            assert_eq!(path, want_path);
            assert_eq!(gateway, ip(want_gw));
        }
        other => panic!("expected UnreachableGateway, got {other:?}"),
    }
}

#[track_caller]
fn assert_family_mismatch(e: Error, want_path: &str) {
    match e {
        Error::FamilyMismatch { path } => assert_eq!(path, want_path),
        other => panic!("expected FamilyMismatch, got {other:?}"),
    }
}

#[track_caller]
fn assert_ipv4_mismatch(e: Error, want_path: &str, want_segment: &str) {
    match e {
        Error::Ipv4Mismatch { path, segment } => {
            assert_eq!(path, want_path);
            assert_eq!(segment, want_segment);
        }
        other => panic!("expected Ipv4Mismatch, got {other:?}"),
    }
}

#[test]
fn baseline_validates() {
    valid().validate().unwrap();
}

#[test]
fn rejects_duplicate_node_name() {
    let mut s = valid();
    s.nodes[1].name = s.nodes[0].name.clone();
    match validate_err(&s) {
        Error::DuplicateNodeName { path, name } => {
            assert_eq!(path, "nodes[1]");
            assert_eq!(name, "a");
        }
        other => panic!("expected DuplicateNodeName, got {other:?}"),
    }
}

#[test]
fn rejects_duplicate_segment_name() {
    let mut s = valid();
    s.segments[1].name = s.segments[0].name.clone();
    match validate_err(&s) {
        Error::DuplicateSegmentName { path, name } => {
            assert_eq!(path, "segments[1]");
            assert_eq!(name, "left");
        }
        other => panic!("expected DuplicateSegmentName, got {other:?}"),
    }
}

#[test]
fn rejects_duplicate_interface_name_on_node() {
    let mut s = valid();
    s.nodes[1].interfaces[1].name = "eth0".into();
    match validate_err(&s) {
        Error::DuplicateInterfaceName { path, name } => {
            assert_eq!(path, "nodes[1].interfaces[1]");
            assert_eq!(name, "eth0");
        }
        other => panic!("expected DuplicateInterfaceName, got {other:?}"),
    }
}

#[test]
fn same_interface_name_on_different_nodes_is_fine() {
    valid().validate().unwrap();
}

#[test]
fn rejects_unknown_segment() {
    let mut s = valid();
    s.nodes[0].interfaces[0].segment = "ghost".into();
    match validate_err(&s) {
        Error::UnknownSegment { path, segment } => {
            assert_eq!(path, "nodes[0].interfaces[0]");
            assert_eq!(segment, "ghost");
        }
        other => panic!("expected UnknownSegment, got {other:?}"),
    }
}

// --- dual-stack rules

#[test]
fn ipv6_only_lab_is_accepted() {
    let yaml = "\
segments:
  - name: s
    ipv6: fd64:1::/64
nodes:
  - name: a
    image: i
    vcpus: 1
    memory: 64
    interfaces:
      - name: eth0
        segment: s
        ipv6: fd64:1::2/64
    static_routes:
      - destination: 2001:db8::/32
        gateway: fd64:1::1
  - name: b
    image: i
    vcpus: 1
    memory: 64
    interfaces:
      - name: eth0
        segment: s
        ipv6: fd64:1::1/64
";
    let spec = LabSpec::from_yaml_str(yaml).expect("IPv6-only segments are allowed");
    assert_eq!(spec.segments[0].ipv4, None);
    assert_eq!(spec.nodes[0].interfaces[0].ipv4, None);
}

#[test]
fn mixed_dual_and_ipv6_only_segments_are_accepted() {
    let mut s = valid();
    s.segments[1].ipv4 = None;
    for node in &mut s.nodes {
        for i in &mut node.interfaces {
            if i.segment == "right" {
                i.ipv4 = None;
            }
        }
    }
    s.validate().unwrap();
}

#[test]
fn ipv4_only_segment_is_rejected() {
    let yaml = "\
segments:
  - name: s
    ipv4: 10.0.0.0/24
nodes: []
";
    parse_err_of(yaml);
}

#[test]
fn ipv4_only_interface_is_rejected() {
    parse_err_of(&VALID.replace("        ipv6: fd64:796c:6f73:1::2/64\n", ""));
    parse_err_of(&VALID.replace("    ipv6: fd64:796c:6f73:1::/64\n", ""));
}

#[test]
fn ipv6_field_with_ipv4_address_is_rejected() {
    parse_err_of(&VALID.replace("ipv6: fd64:796c:6f73:1::/64", "ipv6: 10.0.1.0/24"));
    parse_err_of(&VALID.replace("ipv6: fd64:796c:6f73:1::2/64", "ipv6: 10.0.1.2/24"));
}

#[test]
fn ipv4_field_with_ipv6_address_is_rejected() {
    parse_err_of(&VALID.replace("ipv4: 10.0.1.0/24", "ipv4: fd64:796c:6f73:1::/64"));
    parse_err_of(&VALID.replace("ipv4: 10.0.1.2/24", "ipv4: fd64:796c:6f73:1::2/64"));
}

#[test]
fn rejects_interface_ipv4_when_segment_has_none() {
    let mut s = valid();
    s.segments[0].ipv4 = None;
    assert_ipv4_mismatch(validate_err(&s), "nodes[0].interfaces[0]", "left");
}

#[test]
fn rejects_missing_interface_ipv4_when_segment_has_ipv4() {
    let mut s = valid();
    s.nodes[1].interfaces[1].ipv4 = None;
    assert_ipv4_mismatch(validate_err(&s), "nodes[1].interfaces[1]", "right");
}

#[test]
fn rejects_ipv6_outside_segment_prefix() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv6 = "fd64:796c:6f73:9::1/64".parse().unwrap();
    assert_outside(
        validate_err(&s),
        "nodes[1].interfaces[0].ipv6",
        "fd64:796c:6f73:9::1",
        "left",
        "fd64:796c:6f73:1::/64",
    );
}

#[test]
fn rejects_ipv6_of_another_segment() {
    let mut s = valid();
    // b.eth1 is on "right" but uses an address of "left".
    s.nodes[1].interfaces[1].ipv6 = "fd64:796c:6f73:1::7/64".parse().unwrap();
    assert_outside(
        validate_err(&s),
        "nodes[1].interfaces[1].ipv6",
        "fd64:796c:6f73:1::7",
        "right",
        "fd64:796c:6f73:2::/64",
    );
}

#[test]
fn rejects_ipv4_outside_segment_prefix() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv4 = Some("192.168.9.9/24".parse().unwrap());
    assert_outside(
        validate_err(&s),
        "nodes[1].interfaces[0].ipv4",
        "192.168.9.9",
        "left",
        "10.0.1.0/24",
    );
}

#[test]
fn rejects_ipv4_just_past_segment_prefix() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv4 = Some("10.0.2.1/24".parse().unwrap());
    assert_outside(
        validate_err(&s),
        "nodes[1].interfaces[0].ipv4",
        "10.0.2.1",
        "left",
        "10.0.1.0/24",
    );
}

#[test]
fn rejects_duplicate_ipv4_on_segment() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv4 = Some("10.0.1.2/24".parse().unwrap());
    assert_duplicate_ip(validate_err(&s), "nodes[1].interfaces[0]", "10.0.1.2");
}

#[test]
fn rejects_duplicate_ipv6_on_segment() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv6 = "fd64:796c:6f73:1::2/64".parse().unwrap();
    assert_duplicate_ip(
        validate_err(&s),
        "nodes[1].interfaces[0]",
        "fd64:796c:6f73:1::2",
    );
}

#[test]
fn rejects_duplicate_ipv4_with_different_prefix_length() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv4 = Some("10.0.1.2/25".parse().unwrap());
    assert_duplicate_ip(validate_err(&s), "nodes[1].interfaces[0]", "10.0.1.2");
}

#[test]
fn rejects_duplicate_ipv6_with_different_prefix_length() {
    let mut s = valid();
    s.nodes[1].interfaces[0].ipv6 = "fd64:796c:6f73:1::2/112".parse().unwrap();
    assert_duplicate_ip(
        validate_err(&s),
        "nodes[1].interfaces[0]",
        "fd64:796c:6f73:1::2",
    );
}

#[test]
fn rejects_duplicate_ipv6_written_differently_in_yaml() {
    let yaml = VALID.replace(
        "        ipv6: fd64:796c:6f73:1::1/64\n",
        "        ipv6: FD64:796C:6F73:1:0:0:0:2/64\n",
    );
    match err_of(&yaml) {
        Error::DuplicateIp { path, ip } => {
            assert_eq!(path, "nodes[1].interfaces[0]");
            assert_eq!(ip, self::ip("fd64:796c:6f73:1::2"));
        }
        other => panic!("expected DuplicateIp, got {other:?}"),
    }
}

#[test]
fn rejects_duplicate_ipv4_on_two_interfaces_of_same_node() {
    let mut s = valid();
    s.nodes[0].interfaces.push(iface(
        "eth1",
        "left",
        "fd64:796c:6f73:1::9/64",
        Some("10.0.1.2/24"),
    ));
    assert_duplicate_ip(validate_err(&s), "nodes[0].interfaces[1]", "10.0.1.2");
}

#[test]
fn rejects_duplicate_ipv6_on_two_interfaces_of_same_node() {
    let mut s = valid();
    s.nodes[0].interfaces.push(iface(
        "eth1",
        "left",
        "fd64:796c:6f73:1::2/64",
        Some("10.0.1.9/24"),
    ));
    assert_duplicate_ip(
        validate_err(&s),
        "nodes[0].interfaces[1]",
        "fd64:796c:6f73:1::2",
    );
}

#[test]
fn same_ip_on_different_segments_is_allowed() {
    let yaml = "\
segments:
  - name: s1
    ipv6: fd64:1::/64
    ipv4: 10.0.0.0/24
  - name: s2
    ipv6: fd64:2::/64
    ipv4: 10.0.0.0/24
nodes:
  - name: a
    image: i
    vcpus: 1
    memory: 64
    interfaces:
      - name: eth0
        segment: s1
        ipv6: fd64:1::5/64
        ipv4: 10.0.0.5/24
      - name: eth1
        segment: s2
        ipv6: fd64:2::5/64
        ipv4: 10.0.0.5/24
";
    LabSpec::from_yaml_str(yaml).expect("IP uniqueness is per segment");
}

#[test]
fn from_yaml_str_runs_validation() {
    assert!(matches!(
        err_of(&VALID.replace("ipv4: 10.0.1.2/24", "ipv4: 10.9.9.9/24")),
        Error::IpOutsideSegment { .. }
    ));
}

// --- static routes

#[test]
fn rejects_ipv4_destination_with_ipv6_gateway() {
    let mut s = valid();
    s.nodes[0].static_routes[1].gateway = ip("fd64:796c:6f73:1::1");
    assert_family_mismatch(validate_err(&s), "nodes[0].static_routes[1]");
}

#[test]
fn rejects_ipv6_destination_with_ipv4_gateway() {
    let mut s = valid();
    s.nodes[0].static_routes[0].gateway = ip("10.0.1.1");
    assert_family_mismatch(validate_err(&s), "nodes[0].static_routes[0]");
}

#[test]
fn rejects_ipv6_gateway_not_in_any_attached_segment() {
    let mut s = valid();
    s.nodes[0].static_routes[0].gateway = ip("fd64:796c:6f73:5::1");
    assert_unreachable(
        validate_err(&s),
        "nodes[0].static_routes[0]",
        "fd64:796c:6f73:5::1",
    );
}

#[test]
fn rejects_ipv4_gateway_not_in_any_attached_segment() {
    let mut s = valid();
    s.nodes[0].static_routes[1].gateway = ip("10.0.5.1");
    assert_unreachable(validate_err(&s), "nodes[0].static_routes[1]", "10.0.5.1");
}

#[test]
fn rejects_gateway_on_segment_the_node_is_not_attached_to() {
    let mut s = valid();
    s.nodes[0].static_routes[0].gateway = ip("fd64:796c:6f73:2::1");
    assert_unreachable(
        validate_err(&s),
        "nodes[0].static_routes[0]",
        "fd64:796c:6f73:2::1",
    );
    let mut s = valid();
    s.nodes[0].static_routes[1].gateway = ip("10.0.2.1");
    assert_unreachable(validate_err(&s), "nodes[0].static_routes[1]", "10.0.2.1");
}

#[test]
fn gateway_on_any_of_the_nodes_own_segments_is_accepted() {
    let mut s = valid();
    // b is attached to both segments and may use a gateway on either one.
    s.nodes[1].static_routes = vec![
        StaticRoute {
            destination: "10.0.1.0/24".parse().unwrap(),
            gateway: ip("10.0.2.2"),
        },
        StaticRoute {
            destination: "fd64:796c:6f73:1::/64".parse().unwrap(),
            gateway: ip("fd64:796c:6f73:2::2"),
        },
        StaticRoute {
            destination: "172.16.0.0/12".parse().unwrap(),
            gateway: ip("10.0.1.2"),
        },
    ];
    s.validate().unwrap();
}

#[test]
fn gateway_is_checked_against_interface_prefix_ipv4() {
    // 10.0.1.2/25 is on 10.0.1.0/25: 10.0.1.100 is connected, 10.0.1.200 is not,
    // although both lie inside the /24 segment.
    let mut s = valid();
    s.nodes[0].interfaces[0].ipv4 = Some("10.0.1.2/25".parse().unwrap());
    s.nodes[0].static_routes[1].gateway = ip("10.0.1.100");
    s.validate().unwrap();
    s.nodes[0].static_routes[1].gateway = ip("10.0.1.200");
    assert_unreachable(validate_err(&s), "nodes[0].static_routes[1]", "10.0.1.200");
}

#[test]
fn gateway_is_checked_against_interface_prefix_ipv6() {
    // fd64:796c:6f73:1::2/112 covers ::0 to ::ffff; ::1:1 is inside the /64
    // segment but not on the interface's connected subnet.
    let mut s = valid();
    s.nodes[0].interfaces[0].ipv6 = "fd64:796c:6f73:1::2/112".parse().unwrap();
    s.nodes[0].static_routes[0].gateway = ip("fd64:796c:6f73:1::ff");
    s.validate().unwrap();
    s.nodes[0].static_routes[0].gateway = ip("fd64:796c:6f73:1::1:1");
    assert_unreachable(
        validate_err(&s),
        "nodes[0].static_routes[0]",
        "fd64:796c:6f73:1::1:1",
    );
}

#[test]
fn rejects_gateway_equal_to_own_ipv4_address() {
    let mut s = valid();
    s.nodes[0].static_routes[1].gateway = ip("10.0.1.2");
    assert_unreachable(validate_err(&s), "nodes[0].static_routes[1]", "10.0.1.2");
}

#[test]
fn rejects_gateway_equal_to_own_ipv6_address() {
    let mut s = valid();
    s.nodes[0].static_routes[0].gateway = ip("fd64:796c:6f73:1::2");
    assert_unreachable(
        validate_err(&s),
        "nodes[0].static_routes[0]",
        "fd64:796c:6f73:1::2",
    );
}

#[test]
fn rejects_gateway_equal_to_own_address_on_other_interface() {
    let mut s = valid();
    // b's own address on eth1 ("right") used as gateway via the "left" subnet
    // is both not connected there and its own; either way it must be rejected.
    s.nodes[1].static_routes = vec![StaticRoute {
        destination: "172.16.0.0/12".parse().unwrap(),
        gateway: ip("10.0.2.1"),
    }];
    assert_unreachable(validate_err(&s), "nodes[1].static_routes[0]", "10.0.2.1");
}

#[test]
fn route_error_reports_the_failing_route_index() {
    let mut s = valid();
    s.nodes[0].static_routes.push(StaticRoute {
        destination: "172.30.0.0/16".parse().unwrap(),
        gateway: ip("10.0.9.9"),
    });
    assert_unreachable(validate_err(&s), "nodes[0].static_routes[2]", "10.0.9.9");
}

// ---------------------------------------------------------------- 3. errors

#[test]
fn error_for_ip_outside_segment_through_yaml() {
    let yaml = VALID.replace("        ipv4: 10.0.1.1/24\n", "        ipv4: 10.0.7.1/24\n");
    assert_outside(
        err_of(&yaml),
        "nodes[1].interfaces[0].ipv4",
        "10.0.7.1",
        "left",
        "10.0.1.0/24",
    );
}

#[test]
fn error_messages_name_path_and_offending_value() {
    let mut s = valid();
    s.nodes[0].interfaces[0].segment = "ghost".into();
    let m = validate_err(&s).to_string();
    assert!(
        m.contains("nodes[0].interfaces[0]") && m.contains("\"ghost\""),
        "{m}"
    );

    let mut s = valid();
    s.nodes[1].interfaces[0].ipv4 = Some("192.168.9.9/24".parse().unwrap());
    let m = validate_err(&s).to_string();
    for needle in [
        "nodes[1].interfaces[0].ipv4",
        "192.168.9.9",
        "left",
        "10.0.1.0/24",
    ] {
        assert!(m.contains(needle), "{needle} missing from: {m}");
    }

    let mut s = valid();
    s.nodes[0].interfaces[0].ipv4 = Some("10.0.1.2/25".parse().unwrap());
    s.nodes[0].static_routes[1].gateway = ip("10.0.1.200");
    let m = validate_err(&s).to_string();
    assert!(
        m.contains("nodes[0].static_routes[1]") && m.contains("10.0.1.200"),
        "{m}"
    );
}

#[test]
fn parse_errors_keep_line_and_column() {
    let yaml = "\
segments: []
nodes:
  - name: a
    image: alpine
    memory: 64
    interfaces: []
    vcpus: lots
";
    let m = parse_err_of(yaml);
    assert!(m.contains('7'), "line number missing: {m}");
    let lower = m.to_lowercase();
    assert!(lower.contains("line"), "no 'line' in: {m}");
    assert!(lower.contains("col"), "no column in: {m}");
}

#[test]
fn syntax_errors_keep_line_and_column() {
    let m = parse_err_of("segments: [\nnodes: }\n");
    let lower = m.to_lowercase();
    assert!(lower.contains("line"), "{m}");
    assert!(lower.contains("col"), "{m}");
}

// ------------------------------------------------------------- 4. proptest

/// Which address families the generated segments carry.
#[derive(Clone, Copy, Debug)]
enum Stack {
    Dual,
    V6Only,
    Mixed,
}

const V6_PREFIX_LENS: [u8; 3] = [64, 112, 120];
const V4_PREFIX_LENS: [u8; 2] = [24, 25];

/// Gateway host numbers sit below 128 and 256 so that every prefix length
/// above covers them, and above every node host number (1..=6).
const GW_V4_HOST: u8 = 100;
const GW_V6_HOST: u16 = 0x64;

fn build(segs: usize, dual: &[bool], masks: &[u8], v6_len: u8, v4_len: u8) -> LabSpec {
    let segments = (0..segs)
        .map(|s| Segment {
            name: format!("seg{s}"),
            ipv6: format!("fd64:1:{s}::/64").parse().unwrap(),
            ipv4: dual[s].then(|| format!("10.0.{s}.0/24").parse().unwrap()),
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
                    ipv6: format!("fd64:1:{s}::{:x}/{v6_len}", n + 1).parse().unwrap(),
                    ipv4: dual[s].then(|| format!("10.0.{s}.{}/{v4_len}", n + 1).parse().unwrap()),
                })
                .collect();
            let static_routes = interfaces
                .first()
                .map(|i| {
                    let s: usize = i.segment[3..].parse().unwrap();
                    let mut routes = vec![StaticRoute {
                        destination: "2001:db8::/32".parse().unwrap(),
                        gateway: ip(&format!("fd64:1:{s}::{GW_V6_HOST:x}")),
                    }];
                    if i.ipv4.is_some() {
                        routes.push(StaticRoute {
                            destination: "172.20.0.0/16".parse().unwrap(),
                            gateway: ip(&format!("10.0.{s}.{GW_V4_HOST}")),
                        });
                    }
                    routes
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

fn valid_spec_with(stack: Stack) -> BoxedStrategy<LabSpec> {
    (
        1usize..=4,
        prop::collection::vec(any::<bool>(), 4),
        prop::collection::vec(any::<u8>(), 1..=6),
        prop::sample::select(&V6_PREFIX_LEN_SLICE[..]),
        prop::sample::select(&V4_PREFIX_LEN_SLICE[..]),
    )
        .prop_map(move |(segs, flags, masks, v6_len, v4_len)| {
            let dual: Vec<bool> = match stack {
                Stack::Dual => vec![true; 4],
                Stack::V6Only => vec![false; 4],
                Stack::Mixed => flags,
            };
            build(segs, &dual, &masks, v6_len, v4_len)
        })
        .boxed()
}

static V6_PREFIX_LEN_SLICE: [u8; 3] = V6_PREFIX_LENS;
static V4_PREFIX_LEN_SLICE: [u8; 2] = V4_PREFIX_LENS;

fn valid_spec() -> BoxedStrategy<LabSpec> {
    prop_oneof![
        valid_spec_with(Stack::Dual),
        valid_spec_with(Stack::V6Only),
        valid_spec_with(Stack::Mixed),
    ]
    .boxed()
}

/// `(node, interface)` coordinates of every interface in `spec`.
fn iface_spots(spec: &LabSpec) -> Vec<(usize, usize)> {
    spec.nodes
        .iter()
        .enumerate()
        .flat_map(|(n, node)| (0..node.interfaces.len()).map(move |i| (n, i)))
        .collect()
}

fn round_trips(spec: &LabSpec) -> Result<(), TestCaseError> {
    prop_assert!(spec.validate().is_ok());
    let yaml = serde_saphyr::to_string(spec).unwrap();
    let back = LabSpec::from_yaml_str(&yaml).unwrap();
    prop_assert_eq!(spec, &back);
    let yaml2 = serde_saphyr::to_string(&back).unwrap();
    prop_assert_eq!(&LabSpec::from_yaml_str(&yaml2).unwrap(), spec);
    Ok(())
}

proptest! {
    #[test]
    fn dual_stack_topologies_validate_and_round_trip(spec in valid_spec_with(Stack::Dual)) {
        prop_assert!(spec.segments.iter().all(|s| s.ipv4.is_some()));
        round_trips(&spec)?;
    }

    #[test]
    fn ipv6_only_topologies_validate_and_round_trip(spec in valid_spec_with(Stack::V6Only)) {
        prop_assert!(spec.segments.iter().all(|s| s.ipv4.is_none()));
        round_trips(&spec)?;
        let yaml = serde_saphyr::to_string(&spec).unwrap();
        prop_assert!(!yaml.contains("ipv4"), "{}", yaml);
    }

    #[test]
    fn mixed_topologies_validate_and_round_trip(spec in valid_spec_with(Stack::Mixed)) {
        round_trips(&spec)?;
    }

    #[test]
    fn mutation_duplicate_address_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
        want_v4 in any::<bool>(),
        same_node in any::<bool>(),
        other_len in any::<bool>(),
    ) {
        let spots = iface_spots(&spec);
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        let victim = spec.nodes[n].interfaces[i].clone();
        prop_assume!(!want_v4 || victim.ipv4.is_some());
        let s: usize = victim.segment[3..].parse().unwrap();

        // The copy reuses the victim's address in one family, with another
        // prefix length, and a fresh unused address in the other family.
        let mut copy = Interface { name: "ethx".into(), ..victim.clone() };
        let want_ip: std::net::IpAddr = if want_v4 {
            let v4 = victim.ipv4.unwrap().addr();
            copy.ipv4 = Some(format!("{v4}/{}", if other_len { 30 } else { 24 }).parse().unwrap());
            copy.ipv6 = format!("fd64:1:{s}::f00/64").parse().unwrap();
            v4.into()
        } else {
            let v6 = victim.ipv6.addr();
            copy.ipv6 = format!("{v6}/{}", if other_len { 120 } else { 64 }).parse().unwrap();
            copy.ipv4 = victim.ipv4.map(|_| format!("10.0.{s}.200/24").parse().unwrap());
            v6.into()
        };

        let mut bad = spec.clone();
        let want_path;
        if same_node {
            bad.nodes[n].interfaces.push(copy);
            want_path = format!("nodes[{n}].interfaces[{}]", bad.nodes[n].interfaces.len() - 1);
            // Keep the offending interface last in validation order.
            let last_node = bad.nodes.len() - 1;
            prop_assume!(n == last_node);
        } else {
            bad.nodes.push(Node {
                name: "intruder".into(),
                image: "alpine".into(),
                vcpus: 1,
                memory: 64,
                interfaces: vec![copy],
                static_routes: vec![],
            });
            want_path = format!("nodes[{}].interfaces[0]", bad.nodes.len() - 1);
        }
        match bad.validate() {
            Err(Error::DuplicateIp { path, ip }) => {
                prop_assert_eq!(path, want_path);
                prop_assert_eq!(ip, want_ip);
            }
            other => prop_assert!(false, "expected DuplicateIp, got {:?}", other),
        }
        let yaml = serde_saphyr::to_string(&bad).unwrap();
        prop_assert!(LabSpec::from_yaml_str(&yaml).is_err());
    }

    #[test]
    fn mutation_wrong_family_gateway_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let routes: Vec<(usize, usize)> = spec.nodes.iter().enumerate()
            .flat_map(|(n, node)| (0..node.static_routes.len()).map(move |r| (n, r)))
            .collect();
        prop_assume!(!routes.is_empty());
        let (n, r) = routes[pick.index(routes.len())];
        let mut bad = spec;
        let route = &mut bad.nodes[n].static_routes[r];
        route.gateway = match route.destination {
            // Real, attached-looking addresses of the *other* family.
            ipnet::IpNet::V4(_) => ip("fd64:1:0::64"),
            ipnet::IpNet::V6(_) => ip("10.0.0.100"),
        };
        match bad.validate() {
            Err(Error::FamilyMismatch { path }) => {
                prop_assert_eq!(path, format!("nodes[{n}].static_routes[{r}]"));
            }
            other => prop_assert!(false, "expected FamilyMismatch, got {:?}", other),
        }
    }

    #[test]
    fn mutation_gateway_equal_to_own_address_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let routes: Vec<(usize, usize)> = spec.nodes.iter().enumerate()
            .flat_map(|(n, node)| (0..node.static_routes.len()).map(move |r| (n, r)))
            .collect();
        prop_assume!(!routes.is_empty());
        let (n, r) = routes[pick.index(routes.len())];
        let mut bad = spec;
        let own = {
            let first = &bad.nodes[n].interfaces[0];
            match bad.nodes[n].static_routes[r].destination {
                ipnet::IpNet::V4(_) => std::net::IpAddr::from(first.ipv4.unwrap().addr()),
                ipnet::IpNet::V6(_) => std::net::IpAddr::from(first.ipv6.addr()),
            }
        };
        bad.nodes[n].static_routes[r].gateway = own;
        match bad.validate() {
            Err(Error::UnreachableGateway { path, gateway }) => {
                prop_assert_eq!(path, format!("nodes[{n}].static_routes[{r}]"));
                prop_assert_eq!(gateway, own);
            }
            other => prop_assert!(false, "expected UnreachableGateway, got {:?}", other),
        }
    }

    #[test]
    fn mutation_ip_outside_segment_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
        v4 in any::<bool>(),
    ) {
        let spots = iface_spots(&spec);
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        prop_assume!(!v4 || spec.nodes[n].interfaces[i].ipv4.is_some());
        let mut bad = spec;
        let (field, want_ip) = if v4 {
            bad.nodes[n].interfaces[i].ipv4 = Some("192.168.200.1/24".parse().unwrap());
            ("ipv4", ip("192.168.200.1"))
        } else {
            bad.nodes[n].interfaces[i].ipv6 = "fd99::1/64".parse().unwrap();
            ("ipv6", ip("fd99::1"))
        };
        match bad.validate() {
            Err(Error::IpOutsideSegment { path, ip, .. }) => {
                prop_assert_eq!(path, format!("nodes[{n}].interfaces[{i}].{field}"));
                prop_assert_eq!(ip, want_ip);
            }
            other => prop_assert!(false, "expected IpOutsideSegment, got {:?}", other),
        }
    }

    #[test]
    fn mutation_ipv4_presence_mismatch_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let spots = iface_spots(&spec);
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        let mut bad = spec;
        let had_v4 = bad.nodes[n].interfaces[i].ipv4.is_some();
        let s: usize = bad.nodes[n].interfaces[i].segment[3..].parse().unwrap();
        bad.nodes[n].interfaces[i].ipv4 = if had_v4 {
            None
        } else {
            // The unused address 10.0.s.77 is valid for a dual-stack segment
            // but this segment has no IPv4 prefix.
            Some(format!("10.0.{s}.77/24").parse().unwrap())
        };
        match bad.validate() {
            Err(Error::Ipv4Mismatch { path, .. }) => {
                prop_assert_eq!(path, format!("nodes[{n}].interfaces[{i}]"));
            }
            other => prop_assert!(false, "expected Ipv4Mismatch, got {:?}", other),
        }
    }

    #[test]
    fn mutation_unknown_segment_is_rejected(
        spec in valid_spec(),
        pick in any::<prop::sample::Index>(),
    ) {
        let spots = iface_spots(&spec);
        prop_assume!(!spots.is_empty());
        let (n, i) = spots[pick.index(spots.len())];
        let mut bad = spec;
        bad.nodes[n].interfaces[i].segment = "does-not-exist".into();
        match bad.validate() {
            Err(Error::UnknownSegment { path, segment }) => {
                prop_assert_eq!(path, format!("nodes[{n}].interfaces[{i}]"));
                prop_assert_eq!(segment, "does-not-exist");
            }
            other => prop_assert!(false, "expected UnknownSegment, got {:?}", other),
        }
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
    assert!(
        spec.segments.iter().all(|s| s.ipv4.is_some()),
        "reference lab is dual-stack"
    );

    let (a, b, c) = (&spec.nodes[0], &spec.nodes[1], &spec.nodes[2]);
    let (left, right) = (&spec.segments[0], &spec.segments[1]);
    assert_ne!(left.name, right.name);

    // A on the left segment only, C on the right segment only.
    assert_eq!(a.interfaces.len(), 1);
    assert_eq!(a.interfaces[0].segment, left.name);
    assert_eq!(c.interfaces.len(), 1);
    assert_eq!(c.interfaces[0].segment, right.name);

    // B bridges the two distinct segments, one interface on each.
    assert_eq!(b.interfaces.len(), 2);
    let b_left = b
        .interfaces
        .iter()
        .find(|i| i.segment == left.name)
        .unwrap();
    let b_right = b
        .interfaces
        .iter()
        .find(|i| i.segment == right.name)
        .unwrap();
    assert_ne!(b_left.name, b_right.name);

    // Both families of A's and C's routes go through B's addresses.
    assert_eq!(a.static_routes.len(), 2);
    assert_eq!(c.static_routes.len(), 2);
    let route = |node: &Node, dest: &ipnet::IpNet| -> std::net::IpAddr {
        let hits: Vec<_> = node
            .static_routes
            .iter()
            .filter(|r| &r.destination == dest)
            .collect();
        assert_eq!(hits.len(), 1, "{}: route to {dest}", node.name);
        hits[0].gateway
    };
    let net6 = |s: &Segment| ipnet::IpNet::V6(s.ipv6);
    let net4 = |s: &Segment| ipnet::IpNet::V4(s.ipv4.unwrap());
    assert_eq!(
        route(a, &net6(right)),
        std::net::IpAddr::from(b_left.ipv6.addr())
    );
    assert_eq!(
        route(a, &net4(right)),
        std::net::IpAddr::from(b_left.ipv4.unwrap().addr())
    );
    assert_eq!(
        route(c, &net6(left)),
        std::net::IpAddr::from(b_right.ipv6.addr())
    );
    assert_eq!(
        route(c, &net4(left)),
        std::net::IpAddr::from(b_right.ipv4.unwrap().addr())
    );
}

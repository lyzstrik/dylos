#![allow(clippy::unwrap_used)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::uninlined_format_args)]
use super::*;
use proptest::prelude::*;
use std::net::{Ipv4Addr, Ipv6Addr};

#[allow(clippy::too_many_lines)]
pub fn valid_lab_spec() -> impl Strategy<Value = LabSpec> {
    (
        prop::collection::hash_set("[a-z]{2,5}", 1..5), // segment names
        prop::collection::hash_set("[a-z]{2,5}", 1..5), // node names
        any::<u64>(),                                   // random seed for connections
    )
        .prop_map(|(segment_names, node_names, seed)| {
            let seg_names: Vec<String> = segment_names.into_iter().collect();
            let nod_names: Vec<String> = node_names.into_iter().collect();

            let segments = seg_names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let has_ipv4 = (seed.wrapping_add(i as u64) % 2) == 0;
                    let v6 = Ipv6Addr::new(0xfd64, 0x796c, 0x6f73, i as u16, 0, 0, 0, 0);
                    let ipv6 = ipnet::Ipv6Net::new(v6, 64).unwrap();
                    let ipv4 = if has_ipv4 {
                        let ip = Ipv4Addr::new(10, 0, i as u8, 0);
                        Some(ipnet::Ipv4Net::new(ip, 24).unwrap())
                    } else {
                        None
                    };

                    Segment {
                        name: name.clone(),
                        ipv6,
                        ipv4,
                    }
                })
                .collect::<Vec<_>>();

            let mut nodes = Vec::new();
            for (node_idx, name) in nod_names.iter().enumerate() {
                let mut interfaces = Vec::new();
                for (seg_idx, seg) in segments.iter().enumerate() {
                    // Pseudo-random connection
                    if (seed
                        .wrapping_add(node_idx as u64)
                        .wrapping_add(seg_idx as u64 * 37))
                        % 2
                        == 0
                    {
                        let v6 = Ipv6Addr::new(
                            0xfd64,
                            0x796c,
                            0x6f73,
                            seg_idx as u16,
                            0,
                            0,
                            0,
                            node_idx as u16 + 1,
                        );
                        let ipv6 = ipnet::Ipv6Net::new(v6, 64).unwrap();
                        let ipv4 = seg.ipv4.map(|_| {
                            let ip = Ipv4Addr::new(10, 0, seg_idx as u8, node_idx as u8 + 1);
                            ipnet::Ipv4Net::new(ip, 24).unwrap()
                        });

                        interfaces.push(Interface {
                            name: format!("eth{}", seg_idx),
                            segment: seg.name.clone(),
                            ipv6,
                            ipv4,
                        });
                    }
                }

                // Random static routes. Just use a random external network and route through one of the connected segments.
                let mut static_routes = Vec::new();
                if !interfaces.is_empty() && (seed.wrapping_add(node_idx as u64) % 3 == 0) {
                    let iface = &interfaces[0];
                    let seg_idx = seg_names.iter().position(|s| s == &iface.segment).unwrap();
                    let seg = &segments[seg_idx];

                    let use_ipv4 = seg.ipv4.is_some() && (seed % 2 == 0);

                    if use_ipv4 {
                        let gw_ip = Ipv4Addr::new(10, 0, seg_idx as u8, 254);
                        static_routes.push(StaticRoute {
                            destination: ipnet::IpNet::V4(
                                ipnet::Ipv4Net::new(Ipv4Addr::new(8, 8, 8, 0), 24).unwrap(),
                            ),
                            gateway: std::net::IpAddr::V4(gw_ip),
                        });
                    } else {
                        let gw_ip =
                            Ipv6Addr::new(0xfd64, 0x796c, 0x6f73, seg_idx as u16, 0, 0, 0, 254);
                        static_routes.push(StaticRoute {
                            destination: ipnet::IpNet::V6(
                                ipnet::Ipv6Net::new(
                                    Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0),
                                    64,
                                )
                                .unwrap(),
                            ),
                            gateway: std::net::IpAddr::V6(gw_ip),
                        });
                    }
                }

                nodes.push(Node {
                    name: name.clone(),
                    image: "ubuntu".to_string(),
                    vcpus: 1,
                    memory: 512,
                    interfaces,
                    static_routes,
                });
            }

            LabSpec { segments, nodes }
        })
}

#![allow(clippy::unwrap_used)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::uninlined_format_args)]
use super::*;
use proptest::prelude::*;
use std::net::Ipv4Addr;

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
                    let ip = Ipv4Addr::new(10, 0, i as u8, 0);
                    Segment {
                        name: name.clone(),
                        cidr: ipnet::IpNet::V4(ipnet::Ipv4Net::new(ip, 24).unwrap()),
                    }
                })
                .collect::<Vec<_>>();

            let mut nodes = Vec::new();
            for (node_idx, name) in nod_names.iter().enumerate() {
                let mut interfaces = Vec::new();
                for (seg_idx, seg_name) in seg_names.iter().enumerate() {
                    // Pseudo-random connection
                    if (seed
                        .wrapping_add(node_idx as u64)
                        .wrapping_add(seg_idx as u64 * 37))
                        % 2
                        == 0
                    {
                        let ip = Ipv4Addr::new(10, 0, seg_idx as u8, node_idx as u8 + 1);
                        interfaces.push(Interface {
                            name: format!("eth{}", seg_idx),
                            segment: seg_name.clone(),
                            ip: ipnet::IpNet::V4(ipnet::Ipv4Net::new(ip, 24).unwrap()),
                        });
                    }
                }

                // Random static routes. Just use a random external network and route through one of the connected segments.
                let mut static_routes = Vec::new();
                if !interfaces.is_empty() && (seed.wrapping_add(node_idx as u64) % 3 == 0) {
                    let iface = &interfaces[0];
                    let seg_idx = seg_names.iter().position(|s| s == &iface.segment).unwrap();
                    let gw_ip = Ipv4Addr::new(10, 0, seg_idx as u8, 254); // A gateway on that segment
                    static_routes.push(StaticRoute {
                        destination: ipnet::IpNet::V4(
                            ipnet::Ipv4Net::new(Ipv4Addr::new(8, 8, 8, 0), 24).unwrap(),
                        ),
                        gateway: std::net::IpAddr::V4(gw_ip),
                    });
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

            LabSpec { nodes, segments }
        })
}

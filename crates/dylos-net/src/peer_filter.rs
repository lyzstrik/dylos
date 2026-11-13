use std::net::{Ipv4Addr, Ipv6Addr};

use dylos_core::MacAddr;

use crate::FabricPlan;

pub(crate) const TABLE: &str = "dylos_peer_filter";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerFilter {
    tap: String,
    mac: MacAddr,
    ipv6: Ipv6Addr,
    link_local: Ipv6Addr,
    ipv4: Option<Ipv4Addr>,
}

impl PeerFilter {
    pub(crate) fn new(tap: String, mac: MacAddr, ipv6: Ipv6Addr, ipv4: Option<Ipv4Addr>) -> Self {
        let octets = mac.octets();
        let link_local = Ipv6Addr::from([
            0xfe,
            0x80,
            0,
            0,
            0,
            0,
            0,
            0,
            octets[0] ^ 2,
            octets[1],
            octets[2],
            0xff,
            0xfe,
            octets[3],
            octets[4],
            octets[5],
        ]);
        Self {
            tap,
            mac,
            ipv6,
            link_local,
            ipv4,
        }
    }

    fn render(&self, index: usize, rules: &mut Vec<String>) {
        let mac = self.mac;
        let sources = format!("{{ {}, {} }}", self.ipv6, self.link_local);
        rules.push(format!(
            " chain tap{index} {{\n  ether saddr != {mac} counter drop\n"
        ));
        // Accept lists reject VLAN, encapsulation and extension headers, including fragments.
        rules.push("  ether type != { ip6, ip, arp } counter drop\n".into());
        rules.push(format!(
            "  ether type ip6 ip6 saddr != {sources} counter drop\n"
        ));
        rules.push("  ether type ip6 ip6 nexthdr != { tcp, udp, ipv6-icmp } counter drop\n".into());
        rules.push("  ether type ip6 meta l4proto udp udp sport 547 counter drop\n".into());
        // Fixed IPv6 headers make transport offsets unambiguous. NDP is either optionless
        // or has exactly one correctly typed, eight-byte link-layer option (RFC 4861).
        for (kind, option) in [(135, 1), (136, 2)] {
            let target = if kind == 136 {
                format!(
                    " @th,64,128 {{ 0x{:032x}, 0x{:032x} }}",
                    u128::from(self.ipv6),
                    u128::from(self.link_local)
                )
            } else {
                String::new()
            };
            let prefix = format!(
                "  ether type ip6 ip6 nexthdr ipv6-icmp ip6 hoplimit 255 icmpv6 type {kind} icmpv6 code 0{target}"
            );
            rules.push(format!("{prefix} ip6 length 24 counter accept\n"));
            rules.push(format!("{prefix} ip6 length 32 @th,192,16 0x{option:02x}01 @th,208,48 0x{} counter accept\n", mac.to_string().replace(':', "")));
        }
        rules.push("  ether type ip6 meta l4proto ipv6-icmp icmpv6 type { 1, 2, 3, 4, 128, 129 } counter accept\n".into());
        rules.push("  ether type ip6 meta l4proto { tcp, udp } counter accept\n".into());
        if let Some(ip) = self.ipv4 {
            rules.push(format!("  ether type ip ip saddr != {ip} counter drop\n"));
            // No IPv4 options or fragments: neither can obscure a guarded transport header.
            rules.push("  ether type ip ip hdrlength != 5 counter drop\n  ether type ip ip frag-off & 0x3fff != 0 counter drop\n  ether type ip ip protocol udp udp sport 67 counter drop\n  ether type ip ip protocol { tcp, udp, icmp } counter accept\n".into());
            rules.push(format!("  ether type arp arp htype 1 arp ptype ip arp hlen 6 arp plen 4 arp operation {{ request, reply }} arp saddr ether {mac} arp saddr ip {ip} counter accept\n"));
        }
        rules.push("  counter drop\n }\n".into());
    }
}

impl FabricPlan {
    /// Atomic bridge-family batch for the requested per-TAP policy, absent if all TAPs opt out.
    /// This pure renderer does not install or activate protection.
    #[must_use]
    pub fn peer_filter_rules(&self) -> Option<String> {
        if self.peer_filters.is_empty() {
            return None;
        }
        let mut rules = vec![format!("table bridge {TABLE} {{\n")];
        for (index, filter) in self.peer_filters.iter().enumerate() {
            filter.render(index, &mut rules);
        }
        rules.push(
            " chain ingress {\n  type filter hook prerouting priority -300; policy accept;\n"
                .into(),
        );
        for (index, filter) in self.peer_filters.iter().enumerate() {
            rules.push(format!("  iifname \"{}\" jump tap{index}\n", filter.tap));
        }
        rules.push(" }\n}\n".into());
        Some(rules.concat())
    }
}

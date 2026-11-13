# Peer filtering rollout

`peer_filtering` is a lab boolean and an optional segment boolean override.
A segment inherits the lab value when its override is omitted or null; an
explicit segment value wins in either direction. Both fields persist in the
LabSpec stored with snapshots.

Filtering is now on by default, after the enforcement and namespace packet
proof slices. This changes behavior for old YAML specs and snapshot LabSpecs
whose policy field is absent: they now resolve to true. Stored explicit false
values remain false. Restores and forks recreate rules from the persisted
LabSpec before any TAP becomes usable.

Before upgrading, add `peer_filtering: false` to routed or source-spoofing
labs and their persisted LabSpecs when restoring them. Missing trusted nft
or bridge-family support fails lab creation closed; install the documented
host prerequisites before upgrade.

The reference routed lab opts out explicitly because its router forwards
sources owned by other nodes. There is no implicit trusted-router exception.

The strict profile authenticates each TAP's declared MAC, exact global
IPv6 and IPv4 addresses and MAC-derived IPv6 link-local address, guards
router advertisements/redirects and DHCP servers, and rejects fragments,
VLAN/QinQ, unsupported encapsulations and unspecified-source DAD. These
restrictions reduce workload compatibility; source filtering is not a peer
communication ACL. Host rule counters restart on clone
creation and are not part of guest snapshot state.

The initial renderer rejects all IPv6 extension headers (including atomic
fragments), IPv4 header options, and protocols other than TCP, UDP, ICMP,
ICMPv6 and Ethernet/IPv4 ARP. NDP accepts only optionless NS/NA or a single
correctly typed link-layer option matching the declared MAC. This bounded
profile avoids treating unparsed options or extension chains as authenticated.

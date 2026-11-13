# Peer filtering rollout

`peer_filtering` is a lab boolean and an optional segment boolean override.
A segment inherits the lab value when its override is omitted or null; an
explicit segment value wins in either direction. Both fields persist in the
LabSpec stored with snapshots.

During the staged rollout, the lab default is false and the fabric does not
yet enforce this configuration, even when true is requested. Do not rely on
this schema-only release for source authentication. Enforcement and packet
acceptance tests must land before the final default-on change.

The final default-on release will interpret old specs and snapshots without
this field as true. Before upgrading, add `peer_filtering: false` to routed
or source-spoofing labs and their persisted LabSpecs when restoring them.
The reference routed lab opts out explicitly because its router forwards
sources owned by other nodes. There is no implicit trusted-router exception.

The strict profile will authenticate each TAP's declared MAC, exact global
IPv6 and IPv4 addresses and MAC-derived IPv6 link-local address, guard
router advertisements/redirects and DHCP servers, and reject fragments,
VLAN/QinQ, unsupported encapsulations and unspecified-source DAD. These
restrictions reduce workload compatibility; source filtering is not a peer
communication ACL. Host rule counters and token buckets restart on clone
creation and are not part of guest snapshot state.

The initial renderer rejects all IPv6 extension headers (including atomic
fragments), IPv4 header options, and protocols other than TCP, UDP, ICMP,
ICMPv6 and Ethernet/IPv4 ARP. NDP accepts only optionless NS/NA or a single
correctly typed link-layer option matching the declared MAC. This bounded
profile avoids treating unparsed options or extension chains as authenticated.


## Initial rate budgets

Each filtered TAP has independent data and control token buckets. Data
(TCP/UDP and echo/IPv4 ICMP) refills at 10,000 packets/s and 10 MiB/s;
its packet capacity is 1,000. NDP, ARP and ICMPv6 errors use a separate
1,000 packets/s and 256 KiB/s budget with a 100-packet capacity. Exhausting
data tokens does not consume control tokens or another TAP's tokens.

The configured byte bursts are 1 MiB for data and 32 KiB for control.
Native nft byte buckets include one second of quota **plus** the configured
burst: effective capacities are 11 MiB and 288 KiB, respectively. Accepted
traffic must pass both packet and byte buckets; packet tokens can be consumed
by a subsequent byte-limit rejection. Counters and tokens reset per clone.
Rates count nft's bridge skb bytes, rather than wire overhead.

These fixed initial limits can be opted out of only by setting
`peer_filtering: false` at lab or segment level, which also removes source
protection. They are intended for the spike's small labs, not unrestricted
throughput experiments; authenticated peers can still communicate.

### Measurement method and initial evidence

On the development host (Linux 7.2.15-zen1-1-zen, nft 1.1.7), the disposable
unprivileged namespace harness forwarded 10,000 frames of each size with
one attached reader per TAP. Before limits, it measured 82,980 packets/s
for 64-byte Ethernet frames and 124,198,842 bytes/s for 1514-byte frames.
The data packet ceiling is about 12% of the small-frame baseline and the
byte ceiling about 8% of the large-frame baseline. The byte capacity permits
roughly 93 ms at the measured large-frame baseline; the packet cap usually
constrains a burst first.

Twenty pre-limit atomic installs (including an extra nsenter process) had
7.541 ms median / 10.598 ms p95 latency. Full filtered frozen-fabric creation
had 14.091 ms median / 22.850 ms p95. This isolated install measurement is
an upper-bound process benchmark, not a complete VM fork benchmark; 10.598 ms
is 2.12% of the 500 ms per-clone fork budget. The measurement test reruns
both timings after rate rules are added and in E2E CI.

With the final rate rules, the same 20-sample local test measured atomic
installation (including nsenter) at 7.983 ms median / 9.540 ms p95, and full
filtered frozen-fabric creation at 16.292 ms median / 19.312 ms p95. The
install p95 is 1.91% of the 500 ms per-clone budget; this does not replace
the full fork E2E measurement.

# ADR-0002: IPv6-first, dual-stack lab networks

- Status: proposed
- Date: 2026-10-03
- Issue: LYZ-35

## Context

A Dylos lab is a set of microVMs connected by virtual segments, each segment a bridge inside the lab's network namespace (ADR-0001).
The LabSpec (LYZ-14) must say which addresses each segment and interface use, and guest configuration (LYZ-16) and the network tests (LYZ-18) build on that choice.

Labs exist to reproduce realistic environments. Current networks are increasingly IPv6-first, while many tools and workloads still expect IPv4.
A model that only supports one family would either be unrealistic or exclude common workloads, and adding the second family later would change the LabSpec format, the guest configuration and every network test.

## Decision

Lab networks are **IPv6-first and dual-stack by default**.

- Every segment declares an IPv6 prefix. It is required.
- A segment also declares an IPv4 prefix by default: examples, templates and generated labs are dual-stack.
- IPv6-only segments are allowed. IPv4-only segments are rejected by validation.
- An interface has exactly one static address per family its segment declares, inside the segment prefix of that family.
- Static routes are per family: destination and gateway of the same family, gateway on the connected subnet of one of the node's interfaces.
- Code, tests and examples use IPv6 first and also cover IPv4.

### Addressing plan

- IPv6 prefixes are Unique Local Addresses (RFC 4193). The reference labs use the `/48` `fd64:796c:6f73::/48`, one `/64` per segment (`fd64:796c:6f73:1::/64`, `fd64:796c:6f73:2::/64`, ...).
- IPv4 prefixes are private ranges (RFC 1918), one per segment (`10.0.1.0/24`, `10.0.2.0/24`, ...).
- Addresses are static and declared in the LabSpec. No SLAAC, no DHCP or DHCPv6, no privacy extensions: every boot of the same LabSpec gets the same addresses, and so does every clone.
- Clones reuse exactly the same addresses. There is no conflict, because each clone lives in its own network namespace (ADR-0001), which is the same reason MAC addresses and TAP names are reused.
- Using the same `/48` in every lab is acceptable while labs are closed networks. Labs never route these prefixes outside their namespace.

## Consequences

- **LabSpec (LYZ-14):** segments and interfaces carry an `ipv6` field (required) and an `ipv4` field (optional, present by default). Validation is per family.
- **Guest configuration (LYZ-16):** each interface is configured with both addresses at boot, plus per-family routes. IPv6 router advertisements are not used. Duplicate Address Detection should be disabled or made optimistic for these static addresses, so boot does not wait for it; this must be measured against the boot-time budget.
- **Routers (node B in the reference lab):** IPv6 and IPv4 forwarding are both enabled in the guest.
- **Tests (LYZ-18, LYZ-27):** ping and iperf3 flows run over IPv6 first, and IPv4 is covered by the same tests. The long-lived TCP connection that must survive a fork runs over IPv6.
- **Freeze (ADR-0001):** neighbor resolution uses NDP for IPv6 and ARP for IPv4. Neighbor caches and DAD state are guest state and are captured in the snapshot; NDP and ARP frames in transit during the freeze are dropped like any other frame, which both protocols tolerate.
- **Fork:** neighbor caches restored in a clone remain valid, since MAC and IP addresses are identical in every clone.

## What this does not cover

- External connectivity (uplink, NAT, NAT64, global addresses): labs are closed networks during the spike.
- Dynamic addressing (SLAAC, DHCPv6) as a lab feature: possible later, not needed for determinism.
- Per-lab random ULA global IDs: only needed if labs are ever interconnected.
- The exact DAD setting and its effect on boot time: to measure in LYZ-16.

## Alternatives considered

- **IPv4 only.** Simplest and familiar, but unrealistic for current networks, and adding IPv6 later would change the LabSpec format, guest configuration and tests.
- **IPv6 only.** Most modern, but excludes workloads and tools that still assume IPv4, and makes many labs less realistic.
- **Dual-stack with IPv4 first.** Supports both, but keeps IPv6 as a second-class path that tends to be untested. IPv6 first makes it the default path that every test exercises.
- **SLAAC or DHCP for addressing.** Realistic, but addresses would depend on timing and randomness (privacy extensions, DAD), which conflicts with deterministic labs and identical clones.

## References

- RFC 4193, Unique Local IPv6 Unicast Addresses.
- RFC 1918, Address Allocation for Private Internets.
- RFC 4861 (Neighbor Discovery) and RFC 4862 (Stateless Address Autoconfiguration, DAD).
- ADR-0001: consistent snapshots by freezing the fabric.

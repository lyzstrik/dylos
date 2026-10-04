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
- An interface has exactly one LabSpec-assigned static address per family its segment declares, inside the segment prefix of that family. Guests also keep their IPv6 link-local address, which the kernel derives from the interface MAC address.
- Static routes are per family: destination and gateway of the same family, gateway on the connected subnet of one of the node's interfaces.
- Code, tests and examples use IPv6 first and also cover IPv4. Tests select the address family explicitly and never fall back from one family to the other.

### Addressing plan

- IPv6 prefixes are Unique Local Addresses (RFC 4193). The reference labs use the `/48` `fd64:796c:6f73::/48` ("dylos" in hexadecimal), one `/64` per segment (`fd64:796c:6f73:1::/64`, `fd64:796c:6f73:2::/64`, ...). This fixed, mnemonic Global ID is a deliberate exception to the pseudorandom allocation that RFC 4193 section 3.2 requires: it is acceptable only because labs are isolated networks that never exchange these prefixes. If labs are ever interconnected, generate a conforming Global ID once and store it, which keeps boots deterministic and clones identical.
- IPv4 prefixes are private ranges (RFC 1918), one per segment (`10.0.1.0/24`, `10.0.2.0/24`, ...).
- Addresses are static and declared in the LabSpec. No SLAAC, no DHCP or DHCPv6, no privacy extensions: every boot of the same LabSpec gets the same addresses, and so does every clone. Guests do not accept router advertisements and do not autoconfigure addresses, and no router advertisement daemon runs in the lab.
- Link-local addresses are derived from the MAC address (EUI-64), and MAC addresses are deterministic and unique within a segment, so link-local addresses are unique too.
- Duplicate Address Detection is disabled on lab interfaces. Uniqueness is guaranteed before boot instead: LabSpec validation rejects a duplicate address on a segment, and MAC addresses are unique per segment.
- Clones reuse exactly the same addresses. There is no conflict, because each clone lives in its own network namespace (ADR-0001), which is the same reason MAC addresses and TAP names are reused.
- Using the same `/48` in every lab is acceptable while labs are closed networks. Labs never route these prefixes outside their namespace.

## Consequences

- **LabSpec (LYZ-14):** segments and interfaces carry an `ipv6` field (required) and an `ipv4` field (optional, present by default). Validation is per family.
- **Guest configuration (LYZ-16):** each interface is configured with both addresses at boot, plus per-family routes, with DAD disabled, router advertisement acceptance off and autoconfiguration off. Readiness is measured as usable connectivity in both families (a ping across the lab), not as address assignment. Optimistic DAD (RFC 4429) is not an alternative here: an optimistic address may not be used for address resolution, so with cold neighbor caches and no router advertisements communication would still wait for DAD.
- **Routers (node B in the reference lab):** IPv6 and IPv4 forwarding are both enabled in the guest.
- **Tests (LYZ-18, LYZ-27):** ping and iperf3 run with an explicit family (`ping -6` / `iperf3 -6`, then the IPv4 equivalents), with no fallback. Dual-stack does not make applications prefer IPv6: the default address selection policy (RFC 6724 section 2.1) ranks ULA below IPv4. The long-lived TCP connection that must survive a fork runs over IPv6. After a fork, tests also exercise cold neighbor resolution (a new destination), not only traffic over warm neighbor caches.
- **Freeze (ADR-0001):** neighbor resolution uses NDP for IPv6 and ARP for IPv4. Neighbor caches are guest state and are captured in the snapshot. Neighbor solicitations and ARP requests dropped during the freeze are retried by the guests, so ordinary neighbor resolution recovers. DAD would not: it sends a single probe by default (RFC 4862 section 5.1) and a lost probe or reply makes it succeed falsely. This is one more reason DAD is disabled: no DAD exchange can be in flight when a snapshot is taken.
- **Bridges (LYZ-15):** a restored clone gets new bridges, which have no memory of the multicast group memberships the guests reported before the snapshot. The guests' memberships survive in their own state, but they do not report them again. IPv6 neighbor discovery relies on solicited-node multicast, so lab bridges run with multicast snooping disabled for the spike, and multicast is flooded within the segment.
- **Fork:** neighbor caches restored in a clone remain valid, since MAC and IP addresses are identical in every clone. Resuming an already configured guest does not assign addresses again or run DAD.

## What this does not cover

- External connectivity (uplink, NAT, NAT64, global addresses): labs are closed networks during the spike.
- Dynamic addressing (SLAAC, DHCPv6) as a lab feature: possible later, not needed for determinism.
- Per-lab random ULA global IDs: only needed if labs are ever interconnected.
- Boot-time impact of the guest network configuration: to measure in LYZ-16.
- Multicast snooping on lab bridges: disabled for the spike; re-enabling it would require re-learning group memberships after restore.

## Alternatives considered

- **IPv4 only.** Simplest and familiar, but unrealistic for current networks, and adding IPv6 later would change the LabSpec format, guest configuration and tests.
- **IPv6 only.** Most modern, but excludes workloads and tools that still assume IPv4, and makes many labs less realistic.
- **Dual-stack with IPv4 first.** Supports both, but keeps IPv6 as a second-class path that tends to be untested. IPv6 first makes it the default path that every test exercises.
- **SLAAC or DHCP for addressing.** Realistic, but addresses would depend on timing and randomness (privacy extensions, DAD), which conflicts with deterministic labs and identical clones.

## References

- RFC 4193, Unique Local IPv6 Unicast Addresses.
- RFC 1918, Address Allocation for Private Internets.
- RFC 4861 (Neighbor Discovery), RFC 4862 (Stateless Address Autoconfiguration, DAD), RFC 4429 (Optimistic DAD).
- RFC 6724, Default Address Selection for IPv6.
- ADR-0001: consistent snapshots by freezing the fabric.

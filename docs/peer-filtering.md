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

# Architecture

This document provides a high-level overview of the Dylos architecture, its dependency graph, and its core principles.

## Current status of the spike

The following outlines the current implementation status on `main` vs the target design constraints:
- **`dylos-cli`**: Currently a placeholder.
- **`dylos-store`**: Implements the lab directory layout and reflink rootfs copies (LYZ-50). Lab bring-up (LYZ-51) and the CLI (LYZ-52) are still to come.
- **`dylos-runtime`**: Currently launches and shuts down single VMs.
- **Lab-wide features**: No lab-wide snapshot, restore, or fork exists yet (LYZ-19, LYZ-22, LYZ-23).
- **Fabric features**: No fabric freeze/thaw exist yet (LYZ-24).

## Target Design Constraints

The following sections describe the intended target design and constraints for Dylos.

### Crates and Dependency Direction

Dylos is structured as a workspace of crates with a strict unidirectional dependency graph. A crate may never depend on a crate above it in the hierarchy.

- **`dylos-cli`**: The user-facing command-line interface.
- **`dylos-runtime`**: Orchestrates lab lifecycles (up, down, snapshot, restore, fork sequences).
- **`dylos-net`, `dylos-store`, `dylos-fc`**: Manage host resources and interact with the microVM API.
- **`dylos-core`**: Pure domain definitions (LabSpec, manifest parsing, semantic validation). Contains no system-specific or async runtime dependencies.
- **`dylos-agent`**: A standalone binary that runs inside the guest VM to answer health and resync requests via vsock.

Dependency direction:
`dylos-cli` -> `dylos-runtime` -> {`dylos-fc`, `dylos-net`, `dylos-store`} -> `dylos-core`

### Lab Sequences

[ADR-0001: Consistent lab snapshots by freezing the network fabric](adr/0001-consistency-by-fabric-freeze.md) fixes the snapshot and restore ordering; a fork is that restore run N times.
The runtime guarantees that lab-wide steps run concurrently, but each step waits for all VMs in the lab to complete before proceeding to the next step.
These sequences must never be reordered.

### Host Resources and Hygiene

Dylos creates host resources including processes, network namespaces, bridges, TAP devices, sockets, and temporary files.
Resource hygiene is critical: everything created must be destroyed.

- **Creation**: Resources are created deterministically based on the lab ID and the lab specification.
- **Destruction**: Teardown is implemented idempotently and must succeed even if called twice or following a partial failure. On error during a mid-operation state, Dylos attempts to roll back before returning.

### Networking

Dylos lab networks are **IPv6-first and dual-stack** by default, as outlined in [ADR-0002: IPv6-first Dual-stack Networking](adr/0002-ipv6-first-dual-stack.md).
Every segment is assigned an IPv6 prefix and optionally an IPv4 prefix. Addresses are static and explicitly declared in the LabSpec.

### Privilege and Security Model

- **Privileges**: See [ADR-0004: Privilege Model](adr/0004-privilege-model.md) for details on privileged operations.
- **Security**: The overall threat model and security considerations are maintained in the [Threat Model](security/threat-model.md) document. Dylos components are designed with strict boundaries, using `#![forbid(unsafe_code)]` wherever possible (with narrow, documented exceptions in `dylos-net` and `dylos-store`).

## Guest time after restore

Lab boot attaches a Firecracker vsock device (CID 3, jail-local
`/vsock.sock`). Restore binds `vsock.sock_5000` in each new jail before
loading and resuming the VMs. After all VMs resume, the runtime accepts each
agent reconnection, sends the host's current Unix time via `resync`, and
requires an acknowledgement and health reply before thawing the fabric.
An absent or invalid agent fails restore and uses the normal rollback path.
Snapshots made before vsock was configured cannot satisfy this readiness gate.

Resync steps `CLOCK_REALTIME`; it does not explicitly change
`CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`, or TCP timers. Their behavior across
Firecracker restore must be measured rather than inferred from the wall clock.
The dual-stack TCP restore e2e test waits 30 seconds after snapshot completion,
then checks guest wall time against a host time interval with 100 ms tolerance,
checks that monotonic time has not gone backwards, and prints
`CLOCK_MEASUREMENT` with nanosecond clock samples and `ss -o` TCP timer
state before snapshot and after restore. Both pre-existing IPv6 and IPv4 TCP
connections must retain their endpoints and make further progress. This probes
timer recovery for established streaming connections, not every TCP timeout
policy or external peers.

Observed on the Ubuntu 24.04 KVM CI runner with Firecracker 1.17.0
([E2E run](https://github.com/lyzstrik/dylos/actions/runs/38621523007)):
the pre-snapshot to post-resync wall-clock samples advanced 120.278 seconds
(including snapshot work and the deliberate 30-second wait), while monotonic
and boot-time samples advanced 2.292 seconds each. Both clocks remained
nondecreasing, but did not account for the host's elapsed downtime. Restored
applications must therefore not interpret monotonic elapsed time as elapsed
host wall time. Wall time satisfied the 100 ms bound against both host sampling
endpoints after resync.

Before snapshot, the sampled IPv6 data socket had a retransmission timer with
4 ms remaining and zero retransmissions. After restore, both IPv6 and IPv4
data sockets showed retransmission timers with 892 ms remaining and three
retransmissions. Their original endpoints survived and both streams subsequently
progressed. This is evidence of retransmission recovery despite lost fabric
traffic, consistent with guest timer progression following guest monotonic time
rather than the stepped wall clock; it is not a measurement of every timer's
exact remaining deadline at the snapshot cut.

# Architecture

This document provides a high-level overview of the Dylos architecture, its dependency graph, and its core principles.

## Current status of the spike

The following outlines the current implementation status on `main` vs the target design constraints:
- **`dylos-cli`**: Currently a placeholder.
- **`dylos-store`**: Currently being written (LYZ-50).
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

The detailed step-by-step sequences for lab snapshot, restore, and fork operations are defined in [ADR-0001: Consistent Snapshot and Fork](adr/0001-consistency-by-fabric-freeze.md).
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

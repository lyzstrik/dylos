# Threat Model

This document outlines the threat model for the Dylos lab engine.

## Assets

- **Host resources:** compute, memory, network interfaces, and file system.
- **Lab configurations:** `LabSpec` YAML files and snapshot manifest JSON files.
- **Guest execution environment:** Firecracker microVMs and their state.
- **Runtime data:** rootfs, snapshots, virtual disks, and memory files.

## Actors and Trust Level

- **Host operator:** Trusted. Executes the orchestrator and interacts with the host system.
- **LabSpec author:** Trusted to describe a lab's intended configuration, but its strings must never reach a shell, guest kernel command line, or host path unvalidated.
- **Guest VM:** Untrusted. The workloads running inside the microVMs could be malicious or compromised.
- **Network peers inside a lab:** Untrusted. Any VM can send arbitrary traffic to other VMs in the same lab.

## Trust Boundaries

- **LabSpec -> host resources and guest kernel command line:** The configuration provided in a LabSpec must be strictly validated. Unvalidated strings must not be used to construct host paths, shell commands, or the guest kernel command line.
- **Guest -> host via vsock agent:** The in-guest agent communicates with the host orchestrator. The host must strictly parse and limit the size of data received over vsock.
- **Guest -> host via Firecracker:** Firecracker stdout/stderr and metrics read by the host are untrusted and must be parsed safely.
- **API socket:** The Unix socket used for Firecracker API calls must be protected by host file permissions to ensure only authorized local processes can interact with it.
- **Jailer:** Files placed in the chroot jail by a root process must be minimized, and the guest must not be able to traverse or modify host paths outside the jail.

## Main Threats

| Threat | Mitigation | Status |
| --- | --- | --- |
| Injection through LabSpec names: interface names reach the guest kernel command line (`guest_net::boot_args`) and node names reach host resource names | Restrict names to `[A-Za-z0-9-]` with length limits | **Not mitigated yet**: LabSpec validation only checks uniqueness (LYZ-41) |
| Path traversal through snapshot manifest paths | Paths must be relative, without `..`, validated before any file is opened (`SnapshotManifest`) | Mitigated (LYZ-21) |
| Path traversal or unexpected characters in jail ids | Jail ids restricted to the jailer's `[A-Za-z0-9-]` rule | Mitigated in `dylos-runtime` (LYZ-11) |
| Guest escape to the host | Firecracker run by the upstream jailer: chroot, seccomp, own cgroup, unprivileged uid (ADR-0004) | Mitigated by Firecracker and the jailer; one uid shared by all VMs during the spike (per-VM uids are a follow-up) |
| Root orchestrator acting on user-controlled data | Validate every name and path before a root operation; run the orchestrator as root only for the spike (ADR-0004) | Partially: depends on LYZ-41; privileged helper split is a follow-up |
| Malicious data from the guest over vsock | Strict parsing and size limits on the host listener | **Not implemented yet**: the host side of the agent protocol does not exist (LYZ-37). The agent's own `MAX_LINE_BYTES` (4096) only protects the guest |
| Malicious Firecracker stdout/stderr and metrics | Bounded buffers and line limits when the host reads them | **Not mitigated yet**: start errors keep only the last 20 lines, but lines themselves are not size-limited |
| Resource exhaustion from a guest | Firecracker machine limits (vCPU, memory); cgroup per VM | Partially: no cgroup limits are set yet beyond the default `pids.max` |
| Shared kernel file chowned to the jailer uid through hard links | One copy per VM or per-VM uids | **Not mitigated yet** (noted in PR #17) |

## Follow-ups

- Fuzzing of the `LabSpec`, manifest, and agent protocol parsers with `cargo-fuzz` (not done here).

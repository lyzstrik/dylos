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

| Threat | Mitigation / Status |
| --- | --- |
| **Command injection via LabSpec names** | Validated and mitigated (covered by LYZ-41). |
| **Path traversal via LabSpec or Manifest** | Validated paths; chroot jail isolation. |
| **Unauthorized privilege escalation** | Unprivileged Firecracker execution via upstream `jailer`, capabilities model target (covered by ADR-0004). |
| **Malicious guest vsock payloads** | Strict parsing of the agent protocol with size limits (`MAX_LINE_BYTES`). |
| **Malicious Firecracker stdout/stderr and metrics** | **Not mitigated yet.** Strict parsing and bounded read buffers are needed on the host. |
| **Host resource exhaustion (DoS from guest)** | Size limits on guest data and vsock payloads. (Guest constrained by Firecracker limits). |
| **Guest breakout / Host filesystem access** | Chroot jail, seccomp filters, and cgroup isolation via `jailer`. |

## Follow-ups

- Fuzzing of the `LabSpec`, manifest, and agent protocol parsers with `cargo-fuzz` (not done here).

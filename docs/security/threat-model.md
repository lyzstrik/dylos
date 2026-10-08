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
| Injection through LabSpec names: interface names reach the guest kernel command line (`guest_net::boot_args`) and node names reach host resource names | Restrict names to `[A-Za-z0-9-]` with length limits | Mitigated by LYZ-41 (charset and length checked in `LabSpec::validate` and again in `guest_net::boot_args` for the supplied node); keep it lexical: it does not cover filesystem objects (LYZ-43, LYZ-46). |
| Path traversal through snapshot manifest paths | Paths must be relative, without `..`, validated before any file is opened (`SnapshotManifest`) | Mitigated (lexical traversal only) (LYZ-21) |
| Symlink/TOCTOU escape via snapshot manifest or jail population | Trusted, non-writable source/base-directory assumption; jail population checks regular files, opens with `O_NOFOLLOW`, chowns descriptors and verifies device/inode identity (LYZ-43). | Jail population mitigated (LYZ-43); snapshot manifests **not mitigated**: `verify` delegates opening to the caller without a safe opener (LYZ-46). |
| Path traversal or unexpected characters in jail ids | Jail ids restricted to the jailer's `[A-Za-z0-9-]` rule | Mitigated in `dylos-runtime` (LYZ-11) |
| Guest escape to the host | Firecracker run by the upstream jailer: chroot, seccomp, own cgroup, unprivileged uid (ADR-0004) | Risk reduction conditional on trusted binaries and non-root target uid. `VmSpec.netns` is optional; `dylos-net` provides per-lab network namespaces (LYZ-15); passing one to the jailer remains the caller's responsibility. |
| Root orchestrator acting on user-controlled data | Validate every name and path before a root operation; run the orchestrator as root only for the spike (ADR-0004) | Partially: names are validated (LYZ-41); filesystem objects are covered by LYZ-43 and LYZ-46; the privileged helper split is a follow-up |
| Malicious data from the guest over vsock | Strict parsing and size limits on the host listener | **Not implemented yet**: the host side of the agent protocol does not exist (LYZ-37). The agent's own `MAX_LINE_BYTES` (4096) only protects the guest |
| Malicious Firecracker stdout/stderr and metrics | Bounded buffers and line limits when the host reads them | **Not mitigated yet**: start errors keep only the last 20 lines, but lines themselves are not size-limited |
| Resource exhaustion from a guest | Configured guest vCPU/RAM limits; cgroup per VM | Per-VM defaults enforce `pids.max = vCPU count + 32` and `memory.max = guest RAM + 128 MiB` (LYZ-45). Trusted operators can override either limit through `JailerConfig`: an explicit `cgroups` entry replaces only the default of the same property, and removing a limit requires writing `pids.max=max` or `memory.max=max` explicitly. Host CPU, disk/metrics growth and I/O remain unbounded. |
| Shared kernel file chowned to the jailer uid through hard links | One copy per VM or per-VM uids | **Accepted for the spike**, see below (LYZ-47) |
| Malicious Firecracker API responses | Socket directory ownership/permissions assumption. Response caps and internal deadlines. | Mitigated by LYZ-44: response bodies capped at 1 MiB, fault messages capped at a UTF-8 boundary, and a deadline per request (10 s by default, 120 s for snapshot create/load). On timeout the server-side outcome is unknown. The socket directory protection remains a deployment assumption. |
| Untrusted network peers on a segment | Bridge/TAP isolation. Prevention of IPv6 NDP/RA and IPv4 ARP/address spoofing, lateral traffic, and packet flooding. | **Not mitigated**: PR #14 provides bridges/namespaces but not filtering between peers on a segment. `net-setup` disables RA/autoconfiguration but does not authenticate peers or enforce source addresses. **Accepted for the spike**, see below (LYZ-48). |

## Accepted risks during the spike

These two risks are known and deliberately left open during the spike, to keep it simple while it proves consistent snapshots and fast forks. They rely on the spike's conditions: a dedicated host, labs built by the owner, and source files owned by the operator. Both must be revisited after the go, before Dylos runs labs that are not fully trusted.

- **Shared kernel inode owned by the jailer uid (LYZ-47).** Jail population hard-links source files into each jail and chowns them to the jailer uid, and all VMs share that uid. The read-only kernel shared by every VM is therefore owned by that uid on the host: a compromised Firecracker process could modify the kernel used by the other VMs and by future boots. Target: one uid per VM (ADR-0004) and per-VM copies or reflinks of shared files, with source files never chowned.
- **Unfiltered traffic between peers on a segment (LYZ-48).** Bridges isolate labs from each other and from the host, but not VMs on the same segment from each other: a guest can spoof NDP, router advertisements, ARP and source addresses, and flood its peers. Target: per-TAP source filtering, RA and DHCP guard and rate limits, optional per lab since some labs need to model these behaviors.

## Follow-ups

- Fuzzing of the `LabSpec`, manifest, and agent protocol parsers with `cargo-fuzz` (not done here).

# ADR-0004: Privilege model

- Status: proposed
- Date: 2026-10-05
- Issue: LYZ-39

## Context

The runtime (LYZ-11) needs to launch Firecracker microVMs using the `jailer` for isolation.
The upstream `jailer` v1.17.0 forces a specific privilege model: it must start as root in the initial user namespace. It needs root for `chroot`, `mknod` of `/dev/kvm`, `/dev/net/tun` and `/dev/userfaultfd` in the jail, cgroup v2 writes, `setns` into `--netns`, and `setuid`/`setgid`.
After dropping privileges, the Firecracker process it leaves behind runs as the target uid/gid, chrooted, seccomp-filtered, in its own cgroup, and in the lab network namespace.

The problem is how the Dylos orchestrator itself should run to support the upstream jailer. We need to decide whether to run the entire orchestrator as root, run it inside an unprivileged user namespace (rootless), or split the privileges.

## Decision

For the spike, the Dylos orchestrator runs as root and launches the upstream jailer unchanged. Each Firecracker process ends unprivileged: dedicated uid/gid, chroot, seccomp, its own cgroup, and the lab network namespace.

TAP devices are owned by the jailer uid/gid (`TUNSETOWNER`/`TUNSETGROUP`) so Firecracker can open them after dropping privileges.

The target after the spike (post-go) is to split a small privileged helper (for netns/TAP creation, jailer launch, with a narrow local API and using capabilities instead of full root) from an unprivileged orchestrator (handling LabSpec parsing, snapshots, fork, API, CLI), and giving one uid per VM.

## Consequences

- **LYZ-11 (Jailer launch):** The orchestrator runs as root and calls the unmodified upstream jailer.
- **LYZ-15 (Netns/TAP):** TAP devices must be created with `TUNSETOWNER`/`TUNSETGROUP` matching the jailer's uid/gid.
- **LYZ-17 (Lab up/down):** Handled by the root orchestrator.
- **Tests:** Unit tests run unprivileged using fakes. End-to-end (e2e) tests run as root by a human.

## What this does not cover

- Implementation details of the future privileged helper and its local API to communicate with the unprivileged orchestrator.
- Providing one dedicated uid per VM (currently all VMs share the jailer's unprivileged uid during the spike).

## Alternatives considered

- **Full root forever:** Rejected as a long-term solution because running the entire orchestrator (including LabSpec parsing, API, CLI, snapshots, forks) as root exposes a large privileged surface area unnecessarily.
- **Rootless (Dylos in its own user namespace):** Not a goal. It would require replacing the jailer (due to `mknod` and cgroup delegation restrictions in user namespaces) for no tangible benefit on a dedicated lab host or CI runner.
- **Capabilities now:** Splitting the privileged helper using capabilities instead of full root is deferred to post-spike, to keep the current implementation focused and simple.

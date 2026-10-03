# Dylos

Dylos is an engine for realistic multi-machine environments that can be snapshotted and forked consistently.
It runs networks of [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs, freezes them at a single logical instant, and clones them N times. Every clone resumes as if nothing had happened.

## Status

Technical spike. There is no usable code yet.

The spike must prove that a network of microVMs can be snapshotted consistently and forked quickly. Reference topology: `A -- B -- C`, with B acting as a Linux router between two segments, extended to 5 VMs for measurements.

Go criteria:

- consistent snapshot of a 5-VM lab (256 MiB each) in under 1 s,
- fork of one clone in under 500 ms,
- a long-lived TCP connection from A to C survives in 100% of clones,
- no address conflict with 10 clones running at the same time,
- 100 snapshot, fork, destroy cycles without leaking host resources.

Out of scope for now: web UI, multi-host, Windows guests, eBPF, public API.

## How consistency works

The network fabric is frozen before the VMs are paused. No frame can be delivered after the cut; frames in flight are dropped, which Ethernet allows and TCP recovers from by retransmitting. Each clone is then restored into its own network namespace, so clones can reuse the same addresses without conflicting.

## Planned layout

```
crates/
  dylos-core/     Pure domain: LabSpec, validation, snapshot manifest
  dylos-fc/       Typed client for the Firecracker HTTP API
  dylos-net/      Network namespaces, bridges, TAP devices, fabric freeze/thaw
  dylos-store/    Lab directory layout, reflink cloning of disks
  dylos-runtime/  Orchestration: up, down, snapshot, restore, fork
  dylos-agent/    In-guest agent over vsock
  dylos-cli/      The `dylos` binary
xtask/            Build and test tooling
```

## Host requirements

- Linux with KVM (`/dev/kvm` readable and writable by the user)
- Firecracker and jailer at the pinned versions listed in [`docs/host.md`](docs/host.md)
- A filesystem with reflink support (btrfs, or XFS with reflink) for lab data

Check the host with:

```bash
scripts/host-check.sh
```

## Contributing

Work is tracked as `LYZ-xx` issues in Linear. Every change, including documentation, goes through a pull request against `main`.
Read [`AGENTS.md`](AGENTS.md) before contributing; it applies to humans and AI agents alike. Reviews follow [`docs/agents/review.md`](docs/agents/review.md).

## License

Apache License 2.0, see [`LICENSE`](LICENSE).

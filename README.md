# Dylos

[![CI](https://github.com/lyzstrik/dylos/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/lyzstrik/dylos/actions/workflows/ci.yml)

Dylos is an engine for realistic multi-machine environments whose goal is consistent snapshot, fork, and transparent clone resumption.
It aims to run networks of [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs, freeze them at a single logical instant, and clone them N times so that every clone resumes as if nothing had happened.

## Status

Technical spike. `dylos lab up` and `dylos lab down` work for the reference `A -- B -- C` lab (`labs/abc.yaml`). Snapshot and fork are not implemented yet.

The spike must prove that a network of microVMs can be snapshotted consistently and forked quickly. Reference topology: `A -- B -- C`, with B acting as a Linux router between two segments, extended to 5 VMs for measurements.

Go criteria:

- consistent snapshot of a 5-VM lab (256 MiB each) in under 1 s,
- fork of one clone in under 500 ms,
- a long-lived TCP connection from A to C survives in 100% of clones,
- no address conflict with 10 clones running at the same time,
- 100 snapshot, fork, destroy cycles without leaking host resources.

Out of scope for now: web UI, multi-host, Windows guests, eBPF, public API.

## How consistency works

The network fabric is frozen before the VMs are paused. Under the revised freeze contract ([ADR-0001](docs/adr/0001-consistency-by-fabric-freeze.md)), "no frame sent after freeze completion (T) is received by a lab VM before thaw". Frames queued host-side before freeze completion (pre-T) may still be received during the freeze, and captured RX state survives in clones. Frames still host-side at capture are omitted from clones, which use fresh TAP devices in a separate network namespace. Ethernet allows packet loss, and TCP recovers by retransmitting. Capture boundaries, Firecracker device buffers (including deferred RX in `Net::prepare_save`), and the freeze implementation remain unverified on the pinned host with real microVMs. Each clone is then restored into its own network namespace, so clones can reuse the same addresses without conflicting.

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
just host-check
```

## Contributing

Work is tracked as `LYZ-xx` issues in Linear. Every change goes through a pull request against `main`. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for contributor guidelines and [`AGENTS.md`](AGENTS.md) for detailed rules. Reviews follow [`docs/agents/review.md`](docs/agents/review.md).

## Security

See [`SECURITY.md`](SECURITY.md) for our security policy and vulnerability reporting instructions.

## License

Apache License 2.0, see [`LICENSE`](LICENSE).

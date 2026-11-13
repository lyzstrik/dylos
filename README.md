# Dylos

[![CI](https://github.com/lyzstrik/dylos/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/lyzstrik/dylos/actions/workflows/ci.yml)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/15522/badge)](https://www.bestpractices.dev/projects/15522)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/lyzstrik/dylos/badge)](https://scorecard.dev/viewer/?uri=github.com/lyzstrik/dylos)

Dylos is an engine for realistic multi-machine environments whose goal is consistent snapshot, fork, and transparent clone resumption.
It aims to run networks of [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs, freeze them at a single logical instant, and clone them N times so that every clone resumes as if nothing had happened.

## Status

Technical spike. The CLI implements `dylos lab up`, `down`, `list`, `snapshot`, `restore`, `fork`, and `diff` for multi-VM labs, including the reference `A -- B -- C` lab (`labs/abc.yaml`). Lab-wide snapshot and restore use fabric freeze/thaw; fork restores independent clone labs. Snapshots use fs-verity sealing where supported, with full SHA-256 verification as the fallback ([ADR-0005](docs/adr/0005-snapshot-content-verity.md)).

`just bench` runs the spike benchmark suite. See the [benchmark report](docs/bench/spike.md) for results and measurement limits, and the [architecture status](docs/architecture.md) for implementation details. At revision `73383a2`, all measured go criteria passed, as did the separate owner-host E2E checks for TCP survival, clone address isolation, and 100 cycles without resource leaks.

The spike must prove that a network of microVMs can be snapshotted consistently and forked quickly. Reference topology: `A -- B -- C`, with B acting as a Linux router between two segments, extended to 5 VMs for measurements.

Go criteria:

- consistent snapshot of a 5-VM lab (256 MiB each) in under 1 s (pause window, freeze to thaw),
- fork of one clone in under 500 ms,
- a long-lived TCP connection from A to C survives in 100% of clones,
- no address conflict with 10 clones running at the same time,
- 100 snapshot, fork, destroy cycles without leaking host resources.

Out of scope for now: web UI, multi-host, Windows guests, eBPF, public API.

## Benchmark results

[Full report](docs/bench/spike.md): revision [`73383a2`](https://github.com/lyzstrik/dylos/commit/73383a2), 50 repetitions (`dylos-bench bench --repetitions 50`), bare-metal owner host: AMD Ryzen 9 5900HS with Radeon Graphics (16 vCPUs), 15.0 GiB RAM, Linux `7.2.15-zen1-1-zen`, btrfs, Firecracker v1.17.0; 5 VMs, 256 MiB each.

| Measurement | Median | p95 | Go criterion | Result |
| :--- | ---: | ---: | :--- | :--- |
| Snapshot pause window (freeze to thaw) | 561.0 ms | 604.8 ms | < 1000 ms | GO |
| Fork end-to-end (n=1) | 115.5 ms | 118.7 ms | < 500 ms | GO |
| Fork amortized per clone (n=10) | 58.6 ms | 61.1 ms | < 500 ms/clone | GO |
| Fork amortized per clone (n=20) | 58.9 ms | 60.2 ms | < 500 ms/clone | GO |
| Fabric freeze | 2.0 ms | 2.0 ms | < 1000 ms | PASS |
| Pause confirmation skew | 0.085 ms | 0.156 ms | < 100 ms | PASS |
| Per-clone PSS, settled (n=1) | 149.4 MiB | 149.6 MiB | < 256 MiB | PASS |
| Per-clone PSS, settled (n=10) | 52.6 MiB | 54.3 MiB | < 256 MiB | PASS |
| Per-clone PSS, settled (n=20) | 48.3 MiB | 50.1 MiB | < 256 MiB | PASS |

The pause window is dominated by the [ADR-0001 memory copy](docs/adr/0001-consistency-by-fabric-freeze.md): storage-stage p95 594.3 ms of 604.8 ms; snapshot publication completes later (p95 14746.5 ms).
Separate owner-host `just e2e` at `73383a2` passed 21/21 tests: [TCP survival in all ten clones](crates/dylos-runtime/tests/reliability_e2e.rs), [zero address conflict across ten simultaneous clones](crates/dylos-runtime/tests/fork_e2e.rs), and [100 snapshot/fork/destroy cycles without leaks](crates/dylos-runtime/tests/reliability_e2e.rs) (`snapshot_fork_down_endurance`, 1257 s).
That owner-host run has no CI URL; the [E2E workflow](https://github.com/lyzstrik/dylos/actions/workflows/e2e.yml) runs these tests on every PR, with 5 endurance cycles in CI.

## How consistency works

The network fabric is frozen before the VMs are paused. Under the revised freeze contract ([ADR-0001](docs/adr/0001-consistency-by-fabric-freeze.md)), "no frame sent after freeze completion (T) is received by a lab VM before thaw". Frames queued host-side before freeze completion (pre-T) may still be received during the freeze, and captured RX state survives in clones. Frames still host-side at capture are omitted from clones, which use fresh TAP devices in a separate network namespace. Ethernet allows packet loss, and TCP recovers by retransmitting. End-to-end tests exercise fabric freeze under real traffic and restored IPv6 and IPv4 TCP connections; this does not establish exhaustive coverage of Firecracker device buffers or every capture boundary. Each clone is then restored into its own network namespace, so clones can reuse the same addresses without conflicting.

## Repository layout

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

See [`docs/cli.md`](docs/cli.md) for the CLI reference and LabSpec format.

## Getting started

### 1. Obtain the source

Clone the repository and enter the project directory:

```bash
git clone https://github.com/lyzstrik/dylos.git
cd dylos
```

### 2. Host setup

Dylos requires:
- Linux with KVM (`/dev/kvm` readable and writable)
- Pinned, statically linked Firecracker and jailer (`v1.17.0`)
- A filesystem with reflink support (`btrfs`, or `XFS` with reflink)
- Pinned Rust toolchain and developer tools (`just`, `cargo-nextest`, `cargo-deny`, `typos`, etc.)

Follow [`docs/host.md`](docs/host.md) for full installation and host configuration details. Verify that all host prerequisites are satisfied:

```bash
just host-check
```

### 3. Build and check

Run all checks, tests, and linters through `just`:

```bash
just check
```

Dylos does not install a system binary. You can run commands directly using `cargo run --release -p dylos-cli -- ...` or compile the release binary:

```bash
cargo build --release -p dylos-cli
```

The resulting binary will be located at `target/release/dylos`. Release builds are recommended because debug builds hash snapshot state roughly 20x slower.

### 4. Prepare guest assets

Before launching a lab, ensure the guest kernel and root filesystem image are available:
- **Guest kernel**: download `kernels/vmlinux.bin` according to the source URL and checksum in [`docs/host.md`](docs/host.md).
- **Guest rootfs**: build `target/images/rootfs.ext4` with rootless Podman:

```bash
just images
```

> [!IMPORTANT]
> The rootfs image (`target/images/rootfs.ext4`), lab storage (default `target/labs`, overridden by `DYLOS_LAB_DIR`), and jail directory (default `target/jails`, overridden by `DYLOS_JAIL_DIR`) must reside on the same filesystem mount to support reflinks (`FICLONE`). Reflinks fail across different mount boundaries (`EXDEV`).

### 5. Running the reference lab

Running a lab configures network namespaces, bridges, and TAP devices, as well as jailer chroots, which requires root privileges. By default, the CLI searches for static Firecracker and jailer binaries in order: `~<SUDO_USER>/.local/share/dylos/bin` (when executed under `sudo` with consistent identity), `$HOME/.local/share/dylos/bin`, `/usr/local/lib/dylos/bin`, and `/opt/dylos/bin` (as described in [`docs/host.md`](docs/host.md)). You can also set `DYLOS_FC_BIN_DIR` to override this directory.

Run with `sudo -E` if using user-installed Cargo or custom environment variables. Invoking the compiled binary directly is preferred because sudoers `secure_path` can override `PATH` and hide user-installed `cargo`:

```bash
# Optional: override the binary search directory if installed elsewhere
# export DYLOS_FC_BIN_DIR="$HOME/.local/share/dylos/bin"

# Bring up the reference 3-node lab (A -- B -- C) with the compiled binary
sudo -E ./target/release/dylos lab up labs/abc.yaml

# Or via cargo (if sudo preserves PATH to cargo in your environment):
sudo -E cargo run --release -p dylos-cli -- lab up labs/abc.yaml
```

`lab up` starts the microVMs, configures the network topology, and stays in the foreground supervising the lab until interrupted (such as with `Ctrl+C`) or torn down from another terminal.

To tear down the lab from another terminal (or to clean up after an exit), run `lab down` with the lab identifier (which defaults to the spec file stem, `abc`):

```bash
# Tear down with the compiled binary
sudo -E ./target/release/dylos lab down abc

# Or via cargo:
sudo -E cargo run --release -p dylos-cli -- lab down abc
```

List persisted labs and their observed lifecycle state with `dylos lab list`. See the [CLI reference](docs/cli.md) for snapshot, restore, fork, and diff usage, or inspect available subcommands with:

```bash
cargo run -p dylos-cli -- lab --help
```

### Troubleshooting

- **Firecracker execution failure inside jailer chroot**:
  If `lab up` fails with:
  ```text
  Failed to exec into Firecracker: No such file or directory
  ```
  your Firecracker binary is dynamically linked. The jailer executes Firecracker inside a minimal, empty chroot jail lacking host shared libraries and the dynamic loader. Use the statically linked (`static-pie`) release binaries described in [`docs/host.md`](docs/host.md). The CLI automatically looks for these binaries in candidate search order (`~<SUDO_USER>/.local/share/dylos/bin` under `sudo`, `$HOME/.local/share/dylos/bin`, `/usr/local/lib/dylos/bin`, and `/opt/dylos/bin`), or you can point directly to them by setting `DYLOS_FC_BIN_DIR`.

- **Cross-device link errors (`EXDEV`)**:
  If reflink preflight fails, your rootfs image, lab store directory, or jail directory reside on different filesystem mounts. Ensure they are colocated on the same reflink-capable filesystem mount (or configure `DYLOS_LAB_DIR` and `DYLOS_JAIL_DIR` to point to that mount).

- **Bug reports and feature requests**:
  If you encounter issues or want to propose a task, file an issue using the [GitHub issue forms](.github/ISSUE_TEMPLATE/) ([Bug Report](.github/ISSUE_TEMPLATE/bug_report.yml), [Task](.github/ISSUE_TEMPLATE/task.md)). For contributor workflow and coding conventions, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Contributing

Work is tracked as `LYZ-xx` issues in Linear. For bug reports and feature requests, use the GitHub [issue templates](.github/ISSUE_TEMPLATE/) ([Bug Report](.github/ISSUE_TEMPLATE/bug_report.yml) or [Task](.github/ISSUE_TEMPLATE/task.md)). Every change goes through a pull request against `main`. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for contributor guidelines and [`AGENTS.md`](AGENTS.md) for detailed rules. Reviews follow [`docs/agents/review.md`](docs/agents/review.md).

## Security

See [`SECURITY.md`](SECURITY.md) for our security policy and vulnerability reporting instructions.

## License

Apache License 2.0, see [`LICENSE`](LICENSE).

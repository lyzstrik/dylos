# Dylos

[![CI](https://github.com/lyzstrik/dylos/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/lyzstrik/dylos/actions/workflows/ci.yml)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/15522/badge)](https://www.bestpractices.dev/projects/15522)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/lyzstrik/dylos/badge)](https://scorecard.dev/viewer/?uri=github.com/lyzstrik/dylos)

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

Running a lab configures network namespaces, bridges, and TAP devices, as well as jailer chroots, which requires root privileges. If your static Firecracker and jailer binaries are installed in `~/.local/share/dylos/bin` (as described in [`docs/host.md`](docs/host.md)), export `DYLOS_FC_BIN_DIR` so Dylos locates them instead of defaulting to `/usr/bin`.

Run with `sudo -E` to preserve `DYLOS_FC_BIN_DIR`. Invoking the compiled binary directly is preferred because sudoers `secure_path` can override `PATH` and hide user-installed `cargo`:

```bash
# Point to your static Firecracker and jailer binaries (installed per docs/host.md)
export DYLOS_FC_BIN_DIR="$HOME/.local/share/dylos/bin"

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

> [!NOTE]
> There is no `lab list` command; labs are managed by their identifier (`abc`, or an explicit name passed via `--id <ID>`). See available CLI subcommands with:
>
> ```bash
> cargo run -p dylos-cli -- lab --help
> ```

### Troubleshooting

- **Firecracker execution failure inside jailer chroot**:
  If `lab up` fails with:
  ```text
  Failed to exec into Firecracker: No such file or directory
  ```
  your Firecracker binary is dynamically linked. The jailer executes Firecracker inside a minimal, empty chroot jail lacking host shared libraries and the dynamic loader. Use the statically linked (`static-pie`) release binaries described in [`docs/host.md`](docs/host.md) and set `DYLOS_FC_BIN_DIR` to their directory (for example `export DYLOS_FC_BIN_DIR="$HOME/.local/share/dylos/bin"`). Without `DYLOS_FC_BIN_DIR`, the CLI defaults to `/usr/bin/firecracker` and `/usr/bin/jailer` (unlike `just host-check`, which checks `~/.local/share/dylos/bin` by default).

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

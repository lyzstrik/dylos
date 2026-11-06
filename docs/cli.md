# Dylos CLI & LabSpec Reference

Reference documentation for the `dylos` command-line interface, configuration environment variables, the `LabSpec` YAML topology format, and the snapshot directory layout.

## Running Dylos

The `dylos` binary is not installed globally by default. Invoke it via Cargo or directly from build artifacts:

```bash
# Recommended: release build (debug builds hash snapshots roughly 20x slower)
cargo run --release -p dylos-cli -- lab <subcommand> [options]

# Or via compiled binary:
target/release/dylos lab <subcommand> [options]
```

### Privileges

Operating network namespaces, TAP devices, and Firecracker jailer chroots requires root privileges on the host. Run commands with `sudo -E`:

```bash
sudo -E cargo run --release -p dylos-cli -- lab up labs/abc.yaml
```

> [!NOTE]
> Do not execute privileged commands inside automated tests or unattended scripts. Development host requirements and privilege models are detailed in [docs/host.md](host.md) and [ADR-0004](adr/0004-privilege-model.md).

### Prerequisites

1. **Guest kernel**: Pinned kernel image located at `kernels/vmlinux.bin` (see [docs/host.md](host.md)).
2. **Rootfs image**: Built using `just images`, defaulting to `target/images/rootfs.ext4`.
3. **Reflink storage**: The rootfs image, lab storage directory, and jail directory must reside on the same filesystem mount supporting reflink copies (`ioctl_ficlone` on btrfs or XFS with reflink enabled).
4. **Firecracker & jailer**: Statically linked upstream binaries (defaulting to `~/.local/share/dylos/bin`, configurable via `DYLOS_FC_BIN_DIR`).

> [!CAUTION]
> **Troubleshooting dynamic binaries:** Distribution packages (such as on Arch Linux) are dynamically linked against glibc. When the jailer copies Firecracker into an isolated chroot without dynamic libraries, execution fails with:
> `Failed to exec into Firecracker: No such file or directory`
> Always use the pinned static upstream binaries described in [docs/host.md](host.md).

---

## Command Reference (`dylos lab`)

The CLI entry point manages lab lifecycles through `dylos lab <subcommand>`.

### Foreground Supervision

Subcommands that run microVM environments (`up`, `restore`, and `fork`) remain attached in the foreground to supervise the running fleet. They register signal handlers for `SIGINT` (Ctrl+C) and `SIGTERM`. Supervised runs continue until interrupted by a signal or until torn down via `dylos lab down <id>` from another terminal.

### Output Streams

- **`stdout`**: Emits stable operator status lines. Automated scripts should parse only `stdout`.
- **`stderr`**: Emits structured diagnostic tracing logs (`tracing` subscriber). Format and log lines on `stderr` are not a stable interface.

---

### `dylos lab up`

Bring up a microVM lab topology and supervise it until teardown.

```text
Usage: dylos lab up [OPTIONS] <SPEC>
```

#### Arguments

- `<SPEC>`: Path to the `LabSpec` YAML topology definition file (e.g. `labs/abc.yaml`). Required.

#### Options

- `--id <ID>`: Lab identifier. Optional. Defaults to the file stem of `<SPEC>` (e.g., `abc` for `labs/abc.yaml`). Must start with an alphanumeric character, contain only ASCII alphanumeric characters or hyphens (`-`), and not exceed 32 characters.

#### Standard Output

On successful bring-up:
```text
lab <id> is up, press Ctrl+C or run `dylos lab down <id>`
```

#### Exit Status

Exits with code `0` when cleanly stopped after teardown completes. Exits with a non-zero code (`1`) on invalid arguments, specification parsing or semantic validation failure, preflight reflink check failure, resource allocation failure, or supervisor error.

---

### `dylos lab down`

Tear down a running or crashed lab environment and release all host resources.

```text
Usage: dylos lab down <ID>
```

#### Arguments

- `<ID>`: Identifier of the lab to tear down. Required. Must satisfy safe name constraints (alphanumeric and hyphens, max 32 characters).

#### Behavior

Connects to the lab control socket (`<store>/<id>/control.sock`) with symlink protections and requests graceful teardown within the teardown timeout (default 30 seconds). If the supervisor is unresponsive or the socket is missing, it falls back to idempotent teardown by name, destroying network namespaces, bridges, TAP devices, and jail directories. Calling `down` on a non-existent lab succeeds idempotently.

#### Standard Output

On successful teardown:
```text
lab <id> is down
```

#### Exit Status

Exits with code `0` on success (including idempotent teardown of non-existent labs). Exits with a non-zero code (`1`) on invalid lab identifiers, path traversal attempts, or unrecoverable filesystem errors.

---

### `dylos lab snapshot`

Capture a consistent snapshot of a running lab's microVM states, memory, and disks.

```text
Usage: dylos lab snapshot [OPTIONS] <ID>
```

#### Arguments

- `<ID>`: Identifier of the active lab to snapshot. Required.

#### Options

- `--snapshot-id <SNAPSHOT_ID>`: Custom snapshot identifier. Optional. Defaults to `snap-<timestamp>` generated by the supervisor. If supplied, must satisfy safe name constraints (alphanumeric and hyphens, max 32 characters).

#### Behavior

Requests a snapshot over the lab control socket with a 180-second deadline. The supervisor executes the consistent snapshot sequence: freezes the network fabric, pauses microVMs, writes state and memory dumps, clones disks via reflink, hashes all artifacts with SHA-256, writes `manifest.json`, resumes microVMs, and thaws the network fabric.

#### Standard Output

On successful snapshot:
```text
lab <id> snapshotted successfully as <snapshot_id>
```

#### Exit Status

Exits with code `0` on success. Exits with a non-zero code (`1`) if the lab is not running, the snapshot identifier is invalid, the operation times out, or disk reflink/state serialization fails.

---

### `dylos lab restore`

Restore a snapshot into a new supervised lab.

```text
Usage: dylos lab restore [OPTIONS] <SNAPSHOT>
```

#### Arguments

- `<SNAPSHOT>`: Path to the snapshot directory containing `manifest.json`. Required.

#### Options

- `--id <ID>`: Identifier for the restored lab. Optional. Defaults to `restore-<snapshot-directory-name>`. Must satisfy safe name constraints (alphanumeric and hyphens, max 32 characters).

#### Behavior

Verifies the snapshot directory inventory and file integrity against `manifest.json` (sizes and SHA-256 hashes). Reflink-clones rootfs disks, creates fresh network namespaces and TAP devices, restores microVMs from memory and state files, and supervises the lab in the foreground.

#### Standard Output

On successful restore:
```text
lab <id> restored, press Ctrl+C or run `dylos lab down <id>`
```

#### Exit Status

Exits with code `0` when cleanly stopped. Exits with a non-zero code (`1`) on invalid identifiers, manifest schema or checksum mismatches, missing snapshot files, restore errors, or supervisor failures.

---

### `dylos lab fork`

Fork a snapshot into $N$ parallel, independent clone labs.

```text
Usage: dylos lab fork [OPTIONS] --count <COUNT> <SNAPSHOT>
```

#### Arguments

- `<SNAPSHOT>`: Path to the snapshot directory containing `manifest.json`. Required.

#### Options

- `-n, --count <COUNT>`: Number of clone labs to fork. Required. Must be an integer between 1 and 1024 inclusive.
- `--prefix <PREFIX>`: Base prefix for clone identifiers (alias: `--id`). Optional. Defaults to the snapshot directory or source lab name (e.g. `abc` from `<base>/snapshots/abc/snap-1`). Generates clone identifiers formatted as `<prefix>-c1`, `<prefix>-c2`, ..., `<prefix>-c<count>`. Each generated identifier must not exceed 32 characters.

#### Behavior

Verifies the snapshot manifest and forks $N$ isolated clone labs concurrently. Each clone receives reflinked disk copies and dedicated network namespaces, allowing identical IP configurations across clones without address conflicts. The CLI reports restoration latency and Proportional Set Size (PSS) memory for each clone, then supervises the fleet until teardown.

#### Standard Output

For each restored clone:
```text
clone <clone_id>: restored in <latency_ms> ms, PSS: <pss_kib> KiB
```

Followed by the readiness prompt:
```text
all <count> clones ready, press Ctrl+C or run `dylos lab down <clone>`
```

#### Exit Status

Exits with code `0` when cleanly stopped. Exits with a non-zero code (`1`) if `count` is 0 or greater than 1024, if a generated clone identifier exceeds 32 characters, if manifest verification fails, or if bring-up fails.

---

## Environment Variables

| Variable | Description | Default |
| --- | --- | --- |
| `DYLOS_FC_BIN_DIR` | Directory containing static `firecracker` and `jailer` binaries. Link to [docs/host.md](host.md). | System lookup (`/usr/bin`) |
| `DYLOS_LAB_DIR` | Base directory for lab state storage, per-VM disks, and snapshots (`<base>/snapshots/...`). | `target/labs` |
| `DYLOS_JAIL_DIR` | Base directory for Firecracker jailer chroots. Must be on the same mount as `DYLOS_LAB_DIR` and images. Short paths keep Unix domain sockets under 108 bytes. | Sibling `jails` alongside lab store (e.g. `target/jails`) |
| `DYLOS_TEARDOWN_TIMEOUT_MS` | Timeout in milliseconds when waiting for supervisor teardown over `control.sock`. | `30000` (30 seconds) |

---

## LabSpec Specification

Labs are defined in YAML files declaring network segments, microVM nodes, interfaces, and static routes.

### Schema & Constraints

- **Safe Name Rules**: Names for segments, nodes, and interfaces must start with an ASCII alphanumeric character (`[a-zA-Z0-9]`), contain only alphanumeric characters and hyphens (`[a-zA-Z0-9-]`), and not be empty.
  - Segment names: maximum 32 characters.
  - Node names: maximum 32 characters.
  - Interface names: maximum 15 characters (Linux network interface limit).
  - Node `image`: alphanumeric characters and hyphens (unbounded length).
- **Uniqueness**: Segment names and node names must be unique within the lab. Interface names must be unique within each node. IP addresses (IPv6 and IPv4) must be unique across all interfaces connected to a given segment.
- **IPv6-First & Dual-Stack ([ADR-0002](adr/0002-ipv6-first-dual-stack.md))**:
  - `ipv6` is mandatory on all segments and interfaces.
  - `ipv4` is optional. If a segment declares an IPv4 CIDR, all interfaces attached to it must declare an IPv4 address on that subnet. If a segment omits IPv4, connected interfaces must omit IPv4.
  - Interface IP addresses must fall within their segment CIDR blocks, and interface prefix lengths cannot be wider than their segment prefix lengths.
- **Static Routes**: Destination and gateway address families must match. Gateways cannot be the node's own interface IP and must be directly reachable via at least one configured interface subnet.
- **Strict Fields**: Unknown fields are strictly rejected (`deny_unknown_fields`).

### Reference Example (`labs/abc.yaml`)

```yaml
segments:
  - name: left
    ipv6: fd64:796c:6f73:1::/64
    ipv4: 10.0.1.0/24
  - name: right
    ipv6: fd64:796c:6f73:2::/64
    ipv4: 10.0.2.0/24

nodes:
  - name: A
    image: alpine
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: left
        ipv6: fd64:796c:6f73:1::2/64
        ipv4: 10.0.1.2/24
    static_routes:
      - destination: fd64:796c:6f73:2::/64
        gateway: fd64:796c:6f73:1::1
      - destination: 10.0.2.0/24
        gateway: 10.0.1.1
  - name: B
    image: alpine
    vcpus: 2
    memory: 512
    interfaces:
      - name: eth0
        segment: left
        ipv6: fd64:796c:6f73:1::1/64
        ipv4: 10.0.1.1/24
      - name: eth1
        segment: right
        ipv6: fd64:796c:6f73:2::1/64
        ipv4: 10.0.2.1/24
  - name: C
    image: alpine
    vcpus: 1
    memory: 256
    interfaces:
      - name: eth0
        segment: right
        ipv6: fd64:796c:6f73:2::2/64
        ipv4: 10.0.2.2/24
    static_routes:
      - destination: fd64:796c:6f73:1::/64
        gateway: fd64:796c:6f73:2::1
      - destination: 10.0.1.0/24
        gateway: 10.0.2.1
```

---

## Snapshot Directory & Manifest Layout

Snapshots are stored on disk under `<DYLOS_LAB_DIR>/snapshots/<lab_id>/<snapshot_id>/`:

```text
target/labs/snapshots/abc/snap-20261105-01/
├── manifest.json
└── vms/
    ├── A/
    │   ├── vm.state       # MicroVM state
    │   ├── vm.mem         # Guest RAM dump
    │   └── rootfs.ext4    # Reflink copy of disk
    ├── B/
    │   ├── vm.state
    │   ├── vm.mem
    │   └── rootfs.ext4
    └── C/
        ├── vm.state
        ├── vm.mem
        └── rootfs.ext4
```

### Manifest Schema (`manifest.json`)

The manifest is serialized JSON validated against version 1 (`format_version: 1`):

```json
{
  "format_version": 1,
  "lab_spec": { ... },
  "vms": {
    "A": {
      "state_file": { "path": "vms/A/vm.state", "sha256": "...", "size": 1048576 },
      "memory_file": { "path": "vms/A/vm.mem", "sha256": "...", "size": 268435456 },
      "disks": [
        { "path": "vms/A/rootfs.ext4", "sha256": "...", "size": 134217728 }
      ]
    }
  },
  "firecracker_version": "1.17.0",
  "host_cpu_model": "AMD Ryzen 9 5900HS",
  "created_at_unix_ms": 1793923200000,
  "step_durations_ms": {
    "freeze": 2,
    "pause": 4,
    "snapshot": 45,
    "resume": 3,
    "thaw": 1
  }
}
```

- Paths are relative to the snapshot directory; absolute paths or paths containing `..` components are rejected.
- All declared files are checked for matching size and SHA-256 before restoration. Undeclared or extraneous entries in the snapshot directory are strictly rejected.

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

Operating network namespaces, TAP devices, and Firecracker jailer chroots requires root privileges on the host. By default, Dylos looks for static Firecracker and jailer binaries in `~/.local/share/dylos/bin` (resolving the invoking user's home under `sudo`), as well as `/usr/local/lib/dylos/bin`. You can override the directory with `DYLOS_FC_BIN_DIR`. Run with `sudo -E`:

```bash
sudo -E cargo run --release -p dylos-cli -- lab up labs/abc.yaml
```

> [!NOTE]
> Do not execute privileged commands inside automated tests or unattended scripts. Development host requirements and privilege models are detailed in [docs/host.md](host.md) and [ADR-0004](adr/0004-privilege-model.md).

### Prerequisites

1. **Guest kernel**: Pinned kernel image located at `kernels/vmlinux.bin` (see [docs/host.md](host.md)).
2. **Rootfs image**: Built using `just images`, defaulting to `target/images/rootfs.ext4`. Reflink-cloned for each VM.
3. **Reflink storage**: Rootfs image, lab storage directory, and jail directory must reside on the same filesystem mount supporting reflink copies (`ioctl_ficlone` on btrfs or XFS with reflink enabled).
4. **Firecracker & jailer**: Statically linked upstream binaries (defaults to `~/.local/share/dylos/bin` per [docs/host.md](host.md), resolving the invoking user's home under `sudo`, followed by `/usr/local/lib/dylos/bin`; no PATH lookup). Override with `DYLOS_FC_BIN_DIR`.

> [!CAUTION]
> **Dynamic binaries fail inside chroots:** Distro packages (e.g. Arch Linux) dynamically link glibc. When the jailer copies Firecracker into an isolated chroot without dynamic libraries, execution fails with `Failed to exec into Firecracker: No such file or directory`. Use pinned static binaries from [docs/host.md](host.md).

---

## Command Reference (`dylos lab`)

The CLI entry point manages lab lifecycles through `dylos lab <subcommand>`.

### Foreground Supervision

Subcommands that run microVM environments (`up`, `restore`, and `fork`) supervise the running fleet in the foreground. They register signal handlers for `SIGINT` (Ctrl+C) and `SIGTERM`. Supervised runs continue until interrupted by a signal or torn down via `dylos lab down <id>` from another terminal.

### Common Options & Exit Codes

- `-h, --help`: Print help information and exit with code `0`. Supported on `dylos`, `dylos lab`, and every subcommand.
- **Exit codes**:
  - `0`: Success (or clean stop after signal; help displayed).
  - `1`: Runtime or validation error (spec validation, preflight reflink failure, resource allocation failure, supervisor timeout/error).
  - `2`: Clap command-line usage or argument parsing error (unknown option, missing required argument, invalid flag value).

### Output Streams

- **`stdout`**: Emits stable operator status lines. Automated scripts should parse only `stdout`.
- **`stderr`**: Emits structured diagnostic tracing logs (`tracing` subscriber). Not a stable interface.

---

### `dylos lab up`

Bring up a microVM lab topology and supervise it until teardown.

```text
Usage: dylos lab up [OPTIONS] <SPEC>
```

#### Arguments & Options

- `<SPEC>`: Path to the `LabSpec` YAML topology definition file (e.g. `labs/abc.yaml`). Required.
- `--id <ID>`: Lab identifier. Optional. Defaults to the file stem of `<SPEC>` (e.g., `abc` for `labs/abc.yaml`). Must satisfy safe name rules (ASCII alphanumeric first, `[a-zA-Z0-9-]`, max 32 chars).
- `-h, --help`: Print help and exit `0`.

#### Standard Output

On successful bring-up:
```text
lab <id> is up, press Ctrl+C or run `dylos lab down <id>`
```

#### Exit Status

Exits with `0` when cleanly stopped after teardown. Exits with `2` on argument parsing or usage errors. Exits with `1` on specification parsing or semantic validation failure, preflight check failure, resource allocation failure, or supervisor error.

---

### `dylos lab down`

Tear down a running or crashed lab environment and release all host resources.

```text
Usage: dylos lab down [OPTIONS] <ID>
```

#### Arguments & Options

- `<ID>`: Identifier of the lab to tear down. Required. Must satisfy safe name rules (max 32 chars).
- `-h, --help`: Print help and exit `0`.

#### Behavior

Connects to the lab control socket (`<store>/<id>/control.sock`) with symlink protections and requests graceful teardown within the teardown timeout (default 30 seconds). If the socket is missing or the connection is refused (`NotFound` or `ConnectionRefused`), it falls back to idempotent teardown by name, destroying network namespaces, bridges, TAP devices, and jail directories. Fallback teardown is **not** triggered on timeouts: an unresponsive supervisor returns an error, and recovery refuses a live owner holding the lab directory lock. Operators must not assume a timed-out teardown released resources. Calling `down` on a non-existent lab succeeds idempotently.

#### Standard Output

On successful teardown:
```text
lab <id> is down
```

#### Exit Status

Exits with `0` on success (including idempotent teardown of non-existent labs). Exits with `2` on argument parsing errors. Exits with `1` on invalid lab identifiers, path traversal attempts, timed-out requests to live supervisors, or unrecoverable filesystem errors.

---

### `dylos lab snapshot`

Capture a consistent snapshot of a running lab's microVM states, memory, and disks.

```text
Usage: dylos lab snapshot [OPTIONS] <ID>
```

#### Arguments & Options

- `<ID>`: Identifier of the active lab to snapshot. Required.
- `--snapshot-id <SNAPSHOT_ID>`: Custom snapshot identifier. Optional. Defaults to `snap-<timestamp>` generated by supervisor. If supplied, must satisfy safe name rules (max 32 chars).
- `-h, --help`: Print help and exit `0`.

#### Behavior

Requests a snapshot over the lab control socket with a 180-second deadline. The supervisor executes the consistent snapshot sequence: freezes the network fabric, pauses microVMs, writes state and memory dumps, clones disks via reflink, hashes all artifacts with SHA-256, writes `manifest.json`, resumes microVMs, and thaws the network fabric. A snapshot timeout leaves supervisor outcome unknown; the CLI explicitly warns not to assume rollback.

#### Standard Output

On successful snapshot:
```text
lab <id> snapshotted successfully as <snapshot_id>
```

#### Exit Status

Exits with `0` on success. Exits with `2` on argument parsing errors. Exits with `1` if the lab is not running, the snapshot identifier is invalid, the operation times out, or serialization/reflink fails.

---

### `dylos lab restore`

Restore a snapshot into a new supervised lab.

```text
Usage: dylos lab restore [OPTIONS] <SNAPSHOT>
```

#### Arguments & Options

- `<SNAPSHOT>`: Path to the snapshot directory containing `manifest.json`. Required.
- `--id <ID>`: Identifier for the restored lab. Optional. Defaults to `restore-<snapshot-directory-name>`. Must satisfy safe name rules (max 32 chars).
- `-h, --help`: Print help and exit `0`.

#### Behavior

Verifies the snapshot directory inventory and file integrity against `manifest.json` (sizes and SHA-256 hashes). Reflink-clones rootfs disks, creates fresh network namespaces and TAP devices, restores microVMs from memory and state files, and supervises the lab in the foreground.

#### Standard Output

On successful restore:
```text
lab <id> restored, press Ctrl+C or run `dylos lab down <id>`
```

#### Exit Status

Exits with `0` when cleanly stopped. Exits with `2` on argument parsing errors. Exits with `1` on invalid identifiers, manifest schema or checksum mismatches, missing snapshot files, restore errors, or supervisor failures.

---

### `dylos lab fork`

Fork a snapshot into $N$ parallel, independent clone labs.

```text
Usage: dylos lab fork [OPTIONS] --count <COUNT> <SNAPSHOT>
```

#### Arguments & Options

- `<SNAPSHOT>`: Path to the snapshot directory containing `manifest.json`. Required.
- `-n, --count <COUNT>`: Number of clone labs to fork. Required. Must be an integer between 1 and 1024 inclusive.
- `--prefix <PREFIX>`: Base prefix for clone identifiers (alias: `--id`). Optional. Defaults to snapshot directory or source lab name. Generates `<prefix>-c1` through `<prefix>-c<count>`. Each generated identifier must not exceed 32 characters.
- `-h, --help`: Print help and exit `0`.

#### Behavior

Verifies snapshot manifest and forks $N$ isolated clone labs concurrently. Each clone receives reflinked disk copies and dedicated network namespaces, allowing identical IP configurations across clones without address conflicts. The CLI reports restoration latency and Proportional Set Size (PSS) memory for each clone, then supervises the fleet until teardown.

#### Standard Output

For each restored clone:
```text
clone <clone_id>: restored in <latency_ms> ms, PSS: <pss_kib> KiB
```

If PSS cannot be measured, the CLI emits the fallback line:
```text
clone <clone_id>: restored in <latency_ms> ms, PSS: unavailable
```

Followed by the readiness prompt:
```text
all <count> clones ready, press Ctrl+C or run `dylos lab down <clone>`
```

#### Exit Status

Exits with `0` when cleanly stopped. Exits with `2` on argument parsing errors (e.g. non-integer `--count`). Exits with `1` if `count` is 0 or greater than 1024, if a generated clone identifier exceeds 32 characters, if manifest verification fails, or if bring-up fails.

---

## Environment Variables

| Variable | Description | Default |
| --- | --- | --- |
| `DYLOS_FC_BIN_DIR` | Directory containing static `firecracker` and `jailer` binaries. Link to [docs/host.md](host.md). | `~/.local/share/dylos/bin` (resolves `SUDO_USER` under `sudo`), `/usr/local/lib/dylos/bin` (no PATH lookup) |
| `DYLOS_LAB_DIR` | Base directory for lab state storage, per-VM disks, and snapshots (`<base>/snapshots/...`). | `target/labs` |
| `DYLOS_JAIL_DIR` | Base directory for Firecracker jailer chroots. Must be on same mount as `DYLOS_LAB_DIR` and images. Short paths keep Unix domain sockets under 108 bytes. | Sibling `jails` alongside lab store (e.g. `target/jails`) |
| `DYLOS_TEARDOWN_TIMEOUT_MS` | Timeout in milliseconds when waiting for supervisor teardown over `control.sock`. | `30000` (30 seconds) |

---

## LabSpec Specification

Labs are defined in YAML files declaring network segments, microVM nodes, interfaces, and static routes. Unknown fields are rejected (`deny_unknown_fields`).

### Field Reference

| Field | Type | Required | Description & Constraints |
| --- | --- | --- | --- |
| `segments` | list | Yes | Declared network segments. |
| `segments[].name` | string | Yes | Unique segment ID. Safe name: ASCII alphanumeric first, `[a-zA-Z0-9-]`, max 32 chars. |
| `segments[].ipv6` | string | Yes | Segment IPv6 CIDR prefix (e.g. `fd64:796c:6f73:1::/64`). Mandatory on every segment. |
| `segments[].ipv4` | string | No | Optional IPv4 CIDR prefix (e.g. `10.0.1.0/24`). Enables dual-stack when present. |
| `nodes` | list | Yes | MicroVM nodes in the topology. |
| `nodes[].name` | string | Yes | Unique node ID. Safe name: ASCII alphanumeric first, `[a-zA-Z0-9-]`, max 32 chars. |
| `nodes[].image` | string | Yes | Safe name: ASCII alphanumeric first, `[a-zA-Z0-9-]`, unbounded length. Note: the CLI does not resolve this string to an image download; the runtime always reflink-clones the Store rootfs image (`config.store.image`, default `target/images/rootfs.ext4`). |
| `nodes[].vcpus` | integer | Yes | Number of vCPUs (`u32`). Core accepts any `u32`; runtime requires `1` or an even count through `32` (`1`, `2`, `4`, ..., `32`). |
| `nodes[].memory` | integer | Yes | Guest RAM in MiB (`u32`). Core accepts any `u32`; runtime requires positive memory (`memory > 0`). |
| `nodes[].interfaces` | list | No | Network interfaces attached to segments. Optional, defaults to empty list `[]`. |
| `nodes[].interfaces[].name` | string | Yes | Interface name. Unique per node. Safe name: max 15 chars (Linux `IFNAMSIZ`). |
| `nodes[].interfaces[].segment` | string | Yes | Target segment. Must reference an existing entry in `segments`. |
| `nodes[].interfaces[].ipv6` | string | Yes | Interface IPv6 CIDR. Must fall within segment's IPv6 CIDR with equal or narrower prefix; unique per segment. |
| `nodes[].interfaces[].ipv4` | string | Conditional | Required if segment declares `ipv4`, forbidden if omitted. Must fall within segment's IPv4 CIDR with equal or narrower prefix; unique per segment. |
| `nodes[].static_routes` | list | No | Static routes. Optional, defaults to empty list `[]`. |
| `nodes[].static_routes[].destination` | string | Yes | Destination CIDR prefix (`IpNet`, IPv4 or IPv6). |
| `nodes[].static_routes[].gateway` | string | Yes | Gateway IP (`IpAddr`). Must match destination IP family, cannot be node's own interface IP, and must be directly reachable via a configured node interface. |

### Schema & Constraints

- **Safe Name Rules**: Non-empty string starting with an ASCII alphanumeric character (`[a-zA-Z0-9]`), containing only ASCII alphanumeric characters and hyphens (`[a-zA-Z0-9-]`).
  - Segments: max 32 chars, unique across lab.
  - Nodes: max 32 chars, unique across lab.
  - Interfaces: max 15 chars, unique per node.
  - Images: safe-name rule applies (nonempty, ASCII alphanumeric first), unbounded length.
- **IPv6-First & Dual-Stack ([ADR-0002](adr/0002-ipv6-first-dual-stack.md))**:
  - `ipv6` is mandatory on all segments and interfaces.
  - `ipv4` is optional. If a segment declares an IPv4 CIDR, all interfaces attached to it must declare an IPv4 address on that subnet. If a segment omits IPv4, connected interfaces must omit IPv4.
  - Interface IP addresses must fall within their segment CIDR blocks, and interface prefix lengths cannot be wider than their segment prefix lengths.
- **Uniqueness**: Segment names and node names must be unique within the lab. Interface names must be unique within each node. IP addresses (IPv6 and IPv4) must be unique across all interfaces connected to a given segment.
- **Static Routes**: Destination and gateway address families must match. Gateways cannot be the node's own interface IP and must be directly reachable via at least one configured interface subnet.
- **Strict Fields**: Unknown fields on any object are strictly rejected (`deny_unknown_fields`).

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

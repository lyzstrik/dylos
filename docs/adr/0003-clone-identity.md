# ADR-0003: Clone identity and state hygiene

- Status: proposed
- Date: 2026-10-14
- Issue: LYZ-26

## Context

Dylos creates reproducible, multi-machine environments using Firecracker microVMs. To fork a running lab, Dylos snapshots every VM at a consistent instant ([ADR-0001](0001-consistency-by-fabric-freeze.md)) and restores the resulting state $N$ times.

When Firecracker takes a snapshot (`PUT /snapshot/create`), it serializes the complete guest memory into a memory file and writes vCPU registers, KVM state, and emulated device states into a microVM state file. Disk backing files are cloned by reflink (`FICLONE`). When $N$ clone microVMs are restored from that snapshot (`PUT /snapshot/load` followed by resume), every clone initially executes with an exact byte-for-byte copy of the parent VM's memory and device state.

Consequently, clones of a single snapshot share:
- **Network identity and transport state:** MAC addresses and static IPv6 and IPv4 addresses ([ADR-0002](0002-ipv6-first-dual-stack.md)), ARP/NDP neighbor caches, in-flight TCP sequence numbers, TCP window parameters, retransmission queues, and socket connection blocks.
- **Host and system identity:** SSH host private and public keys (`/etc/ssh/ssh_host_*_key`), systemd and D-Bus machine IDs (`/etc/machine-id`, `/var/lib/dbus/machine-id`), boot ID (`/proc/sys/kernel/random/boot_id`), and hostname.
- **Ephemeral cryptographic session state:** Active TLS sessions (symmetric session keys, IVs/nonces, session tickets, pre-shared keys), crypto library state (OpenSSL, GnuTLS, Rustls, Go crypto runtime), and protocol nonce counters (e.g. ChaCha20, AES-GCM).
- **Random number generator (RNG) state:**
  - Kernel entropy pools and CSPRNG state (ChaCha20 input and primary CRNG pools, per-CPU batched entropy counters).
  - Userspace pseudo-random number generator state (in-memory PRNG instances, DRBG state in OpenSSL, language runtime random seeds).

In standard environments or production deployments, sharing identity, cryptographic keys, and PRNG states across multiple systems causes severe failures:
1. **Cryptographic breakdown:** Reusing a nonce with the same key in stream ciphers or AEAD modes (AES-GCM, ChaCha20-Poly1305) permits key recovery and plaintext forgery. Cloned TLS or WireGuard sessions connecting to external peers lead to catastrophic cryptographic vulnerabilities.
2. **Network collisions:** Identical MAC and IP addresses on a shared network cause ARP/NDP poisoning, duplicate address conflicts, packet misrouting, and sequence number desynchronization on TCP streams.
3. **Identity confusion and token prediction:** Duplicate SSH host keys allow impersonation or trigger host-key verification warnings. Cloned PRNG states lead to identical session tokens, temporary filenames, and cryptographic secrets across different instances.

We must decide:
- What state is shared between clones and why this is acceptable for isolated labs.
- What usage patterns are strictly forbidden.
- How to mitigate RNG cloning and define an explicit workload boundary so that guest workloads consume diverged entropy upon restore.
- How to verify and test entropy divergence across clones.

## Decision

### 1. Acceptance of shared identity in isolated, disposable labs

Clones of a Dylos snapshot reuse identical MAC addresses, static IPv6 and IPv4 addresses, SSH host keys, system identifiers (`machine-id`), TCP sequence numbers, and in-flight connection state.

This is acceptable **only** because Dylos labs operate under strict containment invariants:
- **Per-clone network namespace isolation:** Every lab and every clone lives entirely inside its own dedicated host network namespace (`netns`), as mandated by ADR-0001, ADR-0002, and ADR-0004. Virtual bridges and TAP devices are isolated host objects belonging solely to that namespace. Clone A's network traffic never traverses or reaches Clone B's bridges or TAPs. Layer-2 and Layer-3 collisions cannot occur.
- **Closed networks:** Spike labs are closed networks with no external uplinks, no host-facing default gateways, and no NAT to the outside world. Outside hosts never see duplicate MAC or IP addresses, and external clients never interact with multiple clones simultaneously.
- **Intra-lab TCP continuity:** TCP connections *within* the lab (between VM A and VM B belonging to the same clone) survive across snapshots through TCP retransmission (ADR-0001). Because both endpoints belong to the same consistent snapshot cut and resume together in the same private namespace, their sequence numbers remain synchronized with each other. Clones never communicate with one another.
- **Disposable test environments:** Dylos labs are ephemeral execution environments created to evaluate branches of execution, run parallel simulations, or execute tests from a shared baseline. They are not long-lived production nodes.

### 2. Strict prohibitions for exposed usage

Any exposure of cloned microVMs beyond their isolated namespace is strictly forbidden:
- **No external network attachments:** Clones must never be bridged into physical host interfaces, host default namespaces, VLANs, macvlans, or external networks where identical MACs or IPs could collide with the host or external systems.
- **No multi-tenant snapshot reuse:** Snapshots must never be shared across tenant boundaries or untrusted workloads.
- **No external crypto session resumption:** Clones must not attempt to resume pre-snapshot TLS, WireGuard, or SSH sessions connected to external hosts outside the lab.
- **No production deployment without re-identification:** If Dylos microVMs are ever adapted for persistent or externally routable infrastructure, persistent identities (`/etc/machine-id`, SSH host keys, interface MACs, static IP assignments) must be regenerated post-restore before network exposure.

### 3. Mitigation plan for entropy and random number generation

To prevent multiple clones from producing identical random numbers and cryptographic secrets, guest entropy pools must diverge across all restored instances, and guest workloads must be synchronized so that no workload consumes stale, cloned random bytes.

#### Hardware-assisted VMGenID (Kernel Mechanism)
- **Firecracker v1.17.0 support:** Firecracker v1.17.0 natively and unconditionally implements the Virtual Machine Generation Identifier (`VMGenID`) device on x86_64 via ACPI (`QEMU0003` / `VMGOFF`) and on AArch64 via DeviceTree (`linux,vmgenid`). Firecracker always enables the device ([Firecracker snapshot-support.md](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/snapshot-support.md#userspace-notifications-of-loading-snapshots); ACPI HID alignment with Linux kernel upstreamed in [Firecracker PR #5760 / CHANGELOG.md](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/CHANGELOG.md)). On snapshot resume (`PUT /snapshot/load` followed by `PATCH /vm` with `state: Resumed`), Firecracker generates a fresh 16-byte cryptographically random Generation ID, writes it to guest memory, and injects an ACPI notification (`0x80`) into the guest before resuming vCPUs.
- **Guest kernel support:** The pinned guest kernel `vmlinux-6.18.51` from Firecracker CI (`docs/host.md`) has `CONFIG_VMGENID=y` and `CONFIG_ACPI=y` built directly into the monolithic kernel (verified from kernel configuration).
- **Kernel reseeding mechanism:** Upstream Linux includes native VMGenID driver support since Linux 5.18 (`drivers/virt/vmgenid.c`). Upon receiving the ACPI notification (`0x80`) from Firecracker, `vmgenid_notify()` reads the new 16-byte identifier and calls `add_vmfork_randomness()` ([Linux `drivers/virt/vmgenid.c#L77`](https://github.com/torvalds/linux/blob/v6.18/drivers/virt/vmgenid.c), [Linux `drivers/char/random.c#L993-L1001`](https://github.com/torvalds/linux/blob/v6.18/drivers/char/random.c)). In Linux 6.18, `add_vmfork_randomness()`:
  1. Calls `add_device_randomness(unique_vm_id, len)` to mix the 16-byte ID into the input entropy pool without crediting entropy count (the ID is unique per fork, but not secret).
  2. If the CRNG is ready (`crng_ready()`), calls `crng_reseed(NULL)` to force-reseed the primary ChaCha20 CRNG immediately and emits a kernel log notice (`"crng reseeded due to virtual machine fork\n"`).
  3. Fires registered notifier callbacks via `blocking_notifier_call_chain(&vmfork_chain, 0, NULL)`.
  It does **not** update `/proc/sys/kernel/random/boot_id` (which remains static and shared across all clones, as noted in Section 1 and [Firecracker `random-for-clones.md#user-space-considerations`](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/random-for-clones.md#user-space-considerations)), nor does it alter per-CPU batched entropy counters.
- **Asynchronous notification and the vCPU resume race:** Injecting an ACPI notification before vCPU resume does **not** guarantee that reseeding finishes before guest workloads execute. As explicitly documented in upstream Firecracker ([Firecracker `random-for-clones.md#linux-kernels-with-vmgenid-support`](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/random-for-clones.md#linux-kernels-with-vmgenid-support)), the guest kernel handles the VMGenID notification asynchronously after vCPUs resume. This creates an inherent race window between vCPU resumption and Linux CSPRNG reseeding: any process executing on a resumed vCPU that reads from `/dev/urandom` or `getrandom(2)` before the ACPI interrupt handler finishes will observe stale, identical random bytes across all clones.
- **Workload boundary and gating:** Because VMGenID reseeding is asynchronous, Dylos defines an explicit two-tier workload boundary:
  - *Ungated execution (pre-reseed window):* Quiescent background tasks present at the snapshot baseline (such as kernel scheduler threads, idle loops, `systemd` event loops, and `dylos-agent` waiting on vsock events) resume execution immediately. These background tasks are permitted to run, but must **not** generate cryptographic keys, nonces, or security-sensitive tokens during this window. Pre-snapshot workloads that draw entropy continuously must either be suspended before snapshotting or explicitly await the reseed gate.
  - *Gated workloads (post-reseed guarantee):* Any post-fork workload dispatched or coordinated by Dylos (such as commands executed by `dylos-agent`, test scripts, or services that generate cryptographic material) is held behind an explicit reseed barrier. Gated workloads are released only after reseeding is confirmed.
- **Boot-stage constraint:** Firecracker documentation notes (`snapshot-support.md#vmgenid-device-limitation`) that taking a snapshot during early kernel boot before interrupt handlers are fully initialized can crash the guest on resume when the notification interrupt is injected. Dylos avoids this: snapshots are taken only after full lab readiness (`Lab::up` completed and guest network verified), ensuring interrupt handlers are active.

#### In-guest agent entropy support (Synchronous Reseed Gate & Fallback)
- **Synchronous reseed gate and fallback mechanism:** To completely eliminate the asynchronous VMGenID race window for gated workloads (and to serve as the required fallback for guest kernels lacking VMGenID), `dylos-agent` implements the synchronous reseeding sequence recommended by upstream Firecracker ([Firecracker `random-for-clones.md#recommendations`](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/random-for-clones.md#recommendations)):
  1. `dylos-agent` runs in the guest with `CAP_SYS_ADMIN` privilege (see [ADR-0004](0004-privilege-model.md)).
  2. Following restore, the host orchestrator connects to `dylos-agent` over vsock (triggered by Firecracker's `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` on the guest side) and supplies 32 bytes (256 bits) of fresh host entropy drawn from `/dev/urandom` on the host.
  3. `dylos-agent` opens `/dev/urandom` with `O_RDWR`.
  4. It calls `ioctl(fd, RNDADDENTROPY, &info)` ([`linux/random.h`](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/random.h), man [random(4)](https://man7.org/linux/man-pages/man4/random.4.html)), passing a `struct rand_pool_info` with `entropy_count = 256` and the 32 host entropy bytes. This mixes the bytes into the Linux kernel input pool and credits the entropy count. (Note: `RNDADDTOENTCNT` only credits entropy counters without supplying entropy bytes or forcing a reseed, and `RNDCLEARPOOL` is a no-op on modern kernels; `RNDADDENTROPY` is the required ioctl to inject bytes).
  5. It then explicitly calls `ioctl(fd, RNDRESEEDCRNG)` ([Linux `drivers/char/random.c#L1377`](https://github.com/torvalds/linux/blob/v6.18/drivers/char/random.c)), forcing the primary ChaCha20 CRNG to reseed from the input pool immediately.
  6. Only after `RNDRESEEDCRNG` completes successfully does `dylos-agent` release the workload gate, unblocking command execution and notifying the orchestrator that the clone is ready for workload traffic.
- **Userspace PRNG caches:** VMGenID and kernel reseeding protect kernel-backed interfaces (`getrandom(2)`, `/dev/urandom`). Userspace applications that maintain internal, long-lived PRNG caches (e.g. OpenSSL DRBG instances initialized before snapshot) will not detect VMGenID or kernel ioctls unless they query `getrandom(2)` on each generation. Workloads running in Dylos labs that generate critical security tokens must rely on `getrandom(2)` or reseed upon fork.

### 4. Test plan

The implementation issue stacked on this ADR will verify clone entropy divergence and exercise the workload boundary with an integration test:
1. **Setup:** Boot a reference lab node (node A in `labs/abc.yaml`) to the ready state. Sample a 32-byte baseline from `/dev/urandom` in the parent VM. Create snapshot $S$.
2. **Fork:** Concurrently restore two independent clones (Clone 1 and Clone 2) in separate network namespaces from snapshot $S$.
3. **Workload boundary verification:**
   - **Gated workload execution:** Through `dylos-agent`, execute a workload task in Clone 1 and Clone 2 that is released strictly through the reseed gate (after `RNDADDENTROPY` + `RNDRESEEDCRNG` completes). The workload samples 32 bytes from `getrandom(2)` and 32 bytes from `/dev/urandom`.
   - **Assertions:**
     - Assert `random_bytes_clone1 != random_bytes_clone2`.
     - Assert neither clone's random output matches the parent VM's pre-snapshot baseline.
   - **Gate ordering assertion:** Verify via agent logs and timestamps that the reseed gate completed before the workload command was dispatched.
4. **Kernel event verification:**
   - Inspect guest kernel `dmesg` logs in Clone 1 and Clone 2 to assert that `crng reseeded due to virtual machine fork` is recorded, confirming kernel-level handling of the VMGenID ACPI notification.

## Consequences

### What becomes easier
- **Near-instantaneous fork (< 500 ms):** Reusing identical MAC and IP addresses inside isolated namespaces eliminates the need for guest network reconfiguration, DHCP renegotiation, or route updates on fork.
- **Deterministic, reproducible testing:** Every clone starts from the exact same logical snapshot state, ensuring deterministic test execution.
- **Hybrid hardware and agent entropy guarantees:** Firecracker v1.17.0 and `vmlinux-6.18.51` support VMGenID natively, ensuring in-kernel CSPRNG reseeding via ACPI. Combined with `dylos-agent`'s synchronous reseed barrier (`RNDADDENTROPY` + `RNDRESEEDCRNG`), clones achieve deterministic, provable divergence before any post-fork workload executes.

### What becomes harder and risks to monitor
- **Strict isolation invariant:** Any configuration failure that leaks a clone's TAP device into the host root namespace or bridges two clones together will cause severe network conflicts.
- **Asynchronous VMGenID race window:** Because the guest kernel handles the VMGenID ACPI notification asynchronously after vCPU resume, any ungated guest code executing immediately after restore could consume stale, shared random bytes. Workloads that generate cryptographic secrets must be dispatched behind the `dylos-agent` reseed gate or explicitly synchronize against the reseed notice.
- **Userspace PRNG caches:** Applications with userspace random pools that bypass the kernel after startup will produce identical outputs across clones. Documentation and workload guidance must advise using kernel-backed entropy sources (`getrandom(2)`).

## What this does not cover

- **External connectivity:** Bridging clones to external physical networks or routing outside the host network namespace is out of scope for the spike.
- **Guest wall-clock synchronization:** Snapshot restore causes wall-clock time dilation. Monotonic and wall-clock handling is addressed separately in LYZ-25 (Firecracker `kvm-clock` / VMClock).
- **Userspace application de-duplication:** Automatically invalidating in-memory application session tokens or flushing application-level caches in arbitrary guest user software.

## Alternatives considered

- **Reconfigure network identities (MAC/IP) on every clone:** Dynamically rewrite MAC and IP addresses inside each clone after restore. Rejected: modifying guest network configuration breaks existing intra-lab TCP connections (contradicting ADR-0001 and ADR-0002), introduces non-deterministic delays in guest userspace, and exceeds the 500 ms fork budget.
- **Rely solely on CPU HWRNG (`RDRAND` / `RDSEED`):** Rely on CPU hardware random instructions mixed into the entropy pool by the guest kernel. Rejected: CPU HWRNG instructions alone do not immediately trigger a full kernel CSPRNG pool reseed upon snapshot restore without an explicit time-shift signal such as VMGenID.
- **Rely exclusively on VMGenID without an agent workload gate:** Rely entirely on the kernel VMGenID ACPI notification to reseed the guest CRNG without gating workloads in userspace. Rejected: as documented in Firecracker's `random-for-clones.md`, VMGenID notification delivery is asynchronous to vCPU resumption, leaving an unmitigated race window where early-executing guest processes can draw duplicate random bytes. An explicit workload gate in `dylos-agent` (performing `RNDADDENTROPY` followed by `RNDRESEEDCRNG` before releasing workloads) is necessary to provide race-free divergence guarantees.
- **Reseed exclusively through userspace without VMGenID:** Rely solely on guest userspace scripts without hardware VMGenID support. Rejected: VMGenID provides standard ACPI notification and kernel-level integration (`add_vmfork_randomness()`) natively supported by Firecracker and Linux 6.18. Using both provides defense-in-depth: kernel-level ACPI reseeding plus an explicit userspace workload barrier.
- **Regenerate SSH host keys and machine IDs on fork:** Run `ssh-keygen -A` and `systemd-machine-id-setup` on every clone restore. Rejected for disposable labs: key generation consumes hundreds of milliseconds of vCPU time, breaks pre-configured intra-lab SSH authentication between nodes, and provides no security benefit inside a closed, private network namespace.

## References

- Firecracker documentation:
  - [Firecracker Snapshotting (`docs/snapshotting/snapshot-support.md#userspace-notifications-of-loading-snapshots`)](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/snapshot-support.md#userspace-notifications-of-loading-snapshots)
  - [Entropy for Clones (`docs/snapshotting/random-for-clones.md`)](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/random-for-clones.md)
  - [Firecracker v1.17.0 Changelog (PR #5760 - VMGenID HID fix)](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/CHANGELOG.md)
- Linux kernel implementation and documentation:
  - VMGenID driver: `drivers/virt/vmgenid.c` and `vmgenid_notify()` (Linux 5.18+)
  - Linux CSPRNG and virtual fork reseeding: `drivers/char/random.c` (`add_vmfork_randomness()`, `crng_reseed()`, `vmfork_chain`) in Linux v6.18 ([`random.c#L993-L1001`](https://github.com/torvalds/linux/blob/v6.18/drivers/char/random.c#L993-L1001))
  - Entropy ioctl interfaces: `RNDADDENTROPY` and `RNDRESEEDCRNG` in `include/uapi/linux/random.h` and `drivers/char/random.c#L1334-L1390`
  - Man pages: [random(4)](https://man7.org/linux/man-pages/man4/random.4.html), [random(7)](https://man7.org/linux/man-pages/man7/random.7.html), [getrandom(2)](https://man7.org/linux/man-pages/man2/getrandom.2.html)
  - Guest kernel configuration: `CONFIG_VMGENID=y`, `CONFIG_ACPI=y` in pinned `vmlinux-6.18.51` (`docs/host.md`)
- Specifications:
  - [Microsoft Virtual Machine Generation Identifier](https://learn.microsoft.com/en-us/windows/win32/hyperv_v2/virtual-machine-generation-identifier)
- Dylos Architecture Decisions:
  - [ADR-0001: Consistent lab snapshots by freezing the network fabric](0001-consistency-by-fabric-freeze.md)
  - [ADR-0002: IPv6-first, dual-stack lab networks](0002-ipv6-first-dual-stack.md)
  - [ADR-0004: Privilege model](0004-privilege-model.md)

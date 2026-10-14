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
- How to mitigate RNG cloning so that guest cryptographic entropy diverges immediately upon restore.
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

To prevent multiple clones from producing identical random numbers and cryptographic secrets, guest entropy pools must diverge immediately upon restore.

#### Hardware-assisted VMGenID (Primary Mitigation)
- **Firecracker v1.17.0 support:** Firecracker v1.17.0 natively and unconditionally implements the Virtual Machine Generation Identifier (`VMGenID`) device on x86_64 via ACPI (and on AArch64 via DeviceTree). Firecracker always enables the device. On snapshot resume (`PUT /snapshot/load` followed by `PATCH /vm` with `state: Resumed`), Firecracker generates a fresh 16-byte cryptographically random Generation ID, writes it to guest memory, and injects an ACPI notification interrupt into the guest before resuming vCPUs (see Firecracker documentation `docs/snapshotting/snapshot-support.md` and `docs/snapshotting/random-for-clones.md`; ACPI HID alignment with Linux kernel upstreamed in PR #5760).
- **Guest kernel support:** The pinned guest kernel `vmlinux-6.18.51` from Firecracker CI (`docs/host.md`) has `CONFIG_VMGENID=y` and `CONFIG_ACPI=y` built directly into the monolithic kernel (verified from kernel configuration).
- **Kernel reseeding mechanism:** Upstream Linux includes native VMGenID driver support since Linux 5.18 (`drivers/virt/vmgenid.c`). Upon receiving the ACPI notification from Firecracker, the kernel VMGenID handler invokes `add_vmfork_randomness()`, which immediately reseeds the primary in-kernel CRNG (ChaCha20 pool), resets per-CPU batched entropy allocations, and updates system boot identifiers.
- **Outcome:** The guest kernel's CSPRNG (`/dev/urandom`, `getrandom(2)`) diverges automatically and immediately across all restored clones without requiring guest userspace software modification.
- **Boot-stage constraint:** Firecracker documentation notes (`#vmgenid-device-limitation`) that taking a snapshot during early kernel boot before interrupt handlers are fully initialized can crash the guest on resume when the notification interrupt is injected. Dylos avoids this: snapshots are taken only after full lab readiness (`Lab::up` completed and guest network verified), ensuring interrupt handlers are active.

#### In-guest agent entropy support (Defense-in-depth & userspace hygiene)
- **Kernel reseeding fallback:** If guest kernels lacking VMGenID are ever evaluated, `dylos-agent` (which reconnects over vsock following Firecracker's `VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` on restore) can fetch 256 bits of host entropy and inject them into `/dev/urandom` using the `RNDADDENTROPY` / `RNDADDTOENTCNT` ioctls.
- **Userspace PRNG caches:** VMGenID and kernel reseeding protect kernel-backed interfaces (`getrandom`, `/dev/urandom`). Userspace applications that maintain internal, long-lived PRNG caches (e.g. OpenSSL DRBG instances initialized before snapshot) will not detect VMGenID unless they query `getrandom(2)` on each generation. Workloads running in Dylos labs that generate critical security tokens must rely on `getrandom(2)` or reseed upon fork.

### 4. Test plan

The implementation issue stacked on this ADR will verify clone entropy divergence with an integration test:
1. **Setup:** Boot a reference lab node (node A in `labs/abc.yaml`) to the ready state. Create snapshot $S$.
2. **Fork:** Restore two independent clones (Clone 1 and Clone 2) in separate network namespaces from snapshot $S$.
3. **Execution:** Immediately upon restore, execute a command in Clone 1 and Clone 2 that reads 32 bytes from `/dev/urandom` (and/or invokes `getrandom(2)`).
4. **Assertions:**
   - Assert `random_bytes_clone1 != random_bytes_clone2`.
   - Assert neither clone's random output matches the 32 bytes sampled from the parent VM before snapshot creation.
   - Verify guest kernel dmesg logs confirm VMGenID notification handling and entropy reseeding.

## Consequences

### What becomes easier
- **Near-instantaneous fork (< 500 ms):** Reusing identical MAC and IP addresses inside isolated namespaces eliminates the need for guest network reconfiguration, DHCP renegotiation, or route updates on fork.
- **Deterministic, reproducible testing:** Every clone starts from the exact same logical snapshot state, ensuring deterministic test execution.
- **Automatic kernel entropy divergence:** Because Firecracker v1.17.0 and `vmlinux-6.18.51` support VMGenID natively, in-kernel CSPRNG reseeding requires zero custom guest scripting or agent intervention on the critical restore path.

### What becomes harder and risks to monitor
- **Strict isolation invariant:** Any configuration failure that leaks a clone's TAP device into the host root namespace or bridges two clones together will cause severe network conflicts.
- **Userspace PRNG caches:** Applications with userspace random pools that bypass the kernel after startup will produce identical outputs across clones. Documentation and workload guidance must advise using kernel-backed entropy sources.

## What this does not cover

- **External connectivity:** Bridging clones to external physical networks or routing outside the host network namespace is out of scope for the spike.
- **Guest wall-clock synchronization:** Snapshot restore causes wall-clock time dilation. Monotonic and wall-clock handling is addressed separately in LYZ-25 (Firecracker `kvm-clock` / VMClock).
- **Userspace application de-duplication:** Automatically invalidating in-memory application session tokens or flushing application-level caches in arbitrary guest user software.

## Alternatives considered

- **Reconfigure network identities (MAC/IP) on every clone:** Dynamically rewrite MAC and IP addresses inside each clone after restore. Rejected: modifying guest network configuration breaks existing intra-lab TCP connections (contradicting ADR-0001 and ADR-0002), introduces non-deterministic delays in guest userspace, and exceeds the 500 ms fork budget.
- **Rely solely on CPU HWRNG (`RDRAND` / `RDSEED`):** Rely on CPU hardware random instructions mixed into the entropy pool by the guest kernel. Rejected: CPU HWRNG instructions alone do not immediately trigger a full kernel CSPRNG pool reseed upon snapshot restore without an explicit time-shift signal such as VMGenID.
- **Reseed RNG exclusively through `dylos-agent` over vsock:** Have `dylos-agent` receive entropy from the host orchestrator and reseed the guest. Rejected as the primary mechanism because it executes asynchronously in guest userspace after vCPUs have already resumed; guest processes executing immediately on resume could consume duplicate random bytes before the agent finishes reseeding. VMGenID reseeds the kernel before or at the moment vCPUs resume.
- **Regenerate SSH host keys and machine IDs on fork:** Run `ssh-keygen -A` and `systemd-machine-id-setup` on every clone restore. Rejected for disposable labs: key generation consumes hundreds of milliseconds of vCPU time, breaks pre-configured intra-lab SSH authentication between nodes, and provides no security benefit inside a closed, private network namespace.

## References

- Firecracker documentation:
  - [Firecracker Snapshotting (`docs/snapshotting/snapshot-support.md`)](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/snapshot-support.md)
  - [Entropy for Clones (`docs/snapshotting/random-for-clones.md`)](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/docs/snapshotting/random-for-clones.md)
  - [Firecracker v1.17.0 Changelog (PR #5760 - VMGenID HID fix)](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/CHANGELOG.md)
- Linux kernel implementation:
  - VMGenID driver: `drivers/virt/vmgenid.c` and `add_vmfork_randomness()` (Linux 5.18+)
  - Guest kernel configuration: `CONFIG_VMGENID=y`, `CONFIG_ACPI=y` in pinned `vmlinux-6.18.51` (`docs/host.md`)
- Specifications:
  - [Microsoft Virtual Machine Generation Identifier](https://learn.microsoft.com/en-us/windows/win32/hyperv_v2/virtual-machine-generation-identifier)
- Dylos Architecture Decisions:
  - [ADR-0001: Consistent lab snapshots by freezing the network fabric](0001-consistency-by-fabric-freeze.md)
  - [ADR-0002: IPv6-first, dual-stack lab networks](0002-ipv6-first-dual-stack.md)
  - [ADR-0004: Privilege model](0004-privilege-model.md)

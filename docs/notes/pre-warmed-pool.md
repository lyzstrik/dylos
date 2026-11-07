# Design Note: Pre-warmed Restore Pool for Instant Clones

## 1. Overview & Architecture

To achieve near-instantaneous microVM fork latency (<15 ms per clone), the pre-warmed restore pool (`RestorePool`) decouples slow clone preparation from user hand-out.

In standard cold fork/restore:
1. Storage reflink (`store.restore`): ~5–15 ms
2. Network namespace, bridges, and TAP allocation (`create_network`): ~10–25 ms
3. Jailer and Firecracker microVM launch: ~30–60 ms
4. Snapshot memory/state loading (`PUT /snapshot/load`): ~20–50 ms
5. VM activation (vCPU resume, vsock agent handshake, network thaw, control socket bind): ~5–15 ms

Cold restore takes ~70–165 ms total. The pre-warmed pool executes steps 1–4 ahead of time, maintaining $N$ paused microVM clones with loaded memory and frozen network fabric. When a clone is requested (`take()`), only step 5 is executed on the critical path, and a background task refills the pool asynchronously to replenish capacity.

## 2. Memory Cost

- **Host RAM per paused clone:** Each pre-warmed clone runs Firecracker microVM processes. Guest memory (`vm.mem`) is mapped read-only until modified; the host kernel page cache deduplicates unmodified pages across pre-warmed clones. Host memory overhead per idle paused clone consists of page tables, KVM vCPU state, emulator device state, and process heap (~5–15 MiB PSS per microVM).
- **Disk usage:** Backing disk files (`rootfs.ext4`) are cloned by reflink (`FICLONE`). Reflinks allocate zero extra blocks on creation and diverge on write (COW).
- **Sizing recommendations:** Pool capacity $N \in [1, 3]$ provides instant forks for interactive spikes without excessive host memory reservation.

## 3. Staleness

- **Guest execution state:** While waiting in the pool, guest vCPUs are paused. No guest userspace processes, kernel scheduler ticks, or cron timers execute. Logical VM state remains frozen at snapshot instant $T_0$.
- **Time dilation:** Host wall-clock time continues to advance, creating an idle skew $\Delta t = T_{\text{handout}} - T_0$. Because guest vCPUs are halted, this idle interval does not cause state divergence.

## 4. ADR-0003 Clone Hygiene at Hand-out

All clone identity and hygiene invariants from [ADR-0003](../adr/0003-clone-identity.md) apply at the exact moment of hand-out, rather than at pre-restore time:
- **VMGenID:** Firecracker generates a fresh 16-byte Generation ID and triggers the ACPI notification upon vCPU resume (`PATCH /vm { state: "Resumed" }`).
- **Host Entropy Reseed:** Hand-out connects to `dylos-agent` over vsock and invokes `inject_entropy()`, drawing 32 cryptographically secure random bytes from host `/dev/urandom`. In the guest, `dylos-agent` mixes these into `/dev/urandom` via `RNDADDENTROPY` and triggers `RNDRESEEDCRNG`, guaranteeing provable CSPRNG divergence for post-fork workloads.
- **Clock Resynchronization:** Hand-out sends current host `unix_time_ns` (`SystemTime::now()`) over vsock via `resync()`. The guest agent steps the system clock, completely eliminating the idle time dilation $\Delta t$.
- **Fabric Thaw:** During pre-restoration, the clone's TAP devices and bridges are placed in a frozen state inside an isolated network namespace. Packets cannot escape or enter. Only upon hand-out is `network.thaw()` called.

## 5. Teardown & Resource Hygiene

- **Unallocated clones:** On pool shutdown or drop, all paused clones in the queue are gracefully destroyed (`vm.shutdown()`, network namespace removal, storage directory unlinking).
- **In-flight refills:** Background refill tasks are aborted and awaited before shutdown completes, preventing orphaned processes or leaked jails.
- **Handed-out clones:** Handed-out clones bind their own `control.sock` and are supervised independently; they can be torn down via `dylos lab down <id>` or process signal.

## 6. Hand-out Latency vs Cold Restore

| Metric | Cold Restore (`dylos lab restore`) | Pre-warmed Hand-out (`dylos lab pool`) | Improvement |
| --- | --- | --- | --- |
| Storage & Reflink | 5–15 ms | 0 ms (pre-warmed) | Pre-computed |
| Netns & TAP setup | 10–25 ms | 0 ms (pre-warmed) | Pre-computed |
| Firecracker launch | 30–60 ms | 0 ms (pre-warmed) | Pre-computed |
| Snapshot load | 20–50 ms | 0 ms (pre-warmed) | Pre-computed |
| Activation & Hygiene | 5–15 ms | 5–15 ms | Critical path |
| **Total Fork Latency** | **70–165 ms** | **5–15 ms** | **~8× to 15× speedup** |

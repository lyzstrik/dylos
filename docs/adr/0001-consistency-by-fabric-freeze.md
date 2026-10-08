# ADR-0001: Consistent lab snapshots by freezing the network fabric

- Status: accepted
- Date: 2026-10-03
- Issue: LYZ-9

**Revision (2026-10-08):** The freeze contract becomes "no frame sent after freeze completion (T) is received by a lab VM before thaw". Pre-T frames may enter captured RX state during the freeze and survive in clones; frames still host-side at capture are lost on clones and may be delayed on the original. Linux v6.18 does not purge the per-file TAP queue on administrative link-down, so link-down alone cannot promise loss of every queued frame. The capture boundaries and freeze implementation remain unverified on the pinned host (see LYZ-19 below).

## Context

A Dylos lab is several Firecracker microVMs connected by a virtual network (the fabric: one bridge per segment, one TAP per interface, all inside the lab's network namespace).
To fork a lab, we snapshot every VM and restore the set N times. The set of per-VM snapshots must describe a state the lab could actually have been in.

Pausing N VMs is never simultaneous. Each `PATCH /vm` call lands a few milliseconds apart, and each `PUT /snapshot/create` captures its VM at yet another instant.
During that window, frames keep moving between VMs through host-side paths: TAP queues, the bridge, and Firecracker's device emulation.
A VM captured late can then contain the effect of a frame (data in its memory, a TCP state change) whose cause is missing from the sender's snapshot, or a frame can be both delivered and still queued for sending.
Restored, such a clone holds an effect without a cause: a state that never existed.
Whether Firecracker v1.17.0 can produce each of these cases depends on device-emulation internals we do not want our correctness to rest on.

This decision is needed now because it fixes the order of the snapshot and restore sequences, which every runtime issue (LYZ-19, LYZ-20, LYZ-22, LYZ-23) builds on.

### Consistent cut

A distributed snapshot is a set of local states, one per process, plus the state of the channels between them.
It is consistent if no message is recorded as received without being recorded as sent (Chandy and Lamport, 1985).
Messages sent but not yet received are allowed: they are in transit. Chandy-Lamport assumes reliable channels, so the classic algorithm records them as channel state and replays them on restore.

In this ADR, a frame is *sent* by a VM once it has left that VM's captured state (guest memory and virtio device state), and *received* once it is part of the receiver's captured state.
Being forwarded by the bridge or counted by a TAP is neither: it is the channel.

## Decision

We obtain a consistent cut by freezing the fabric before pausing any VM, and we omit frames still in host-side channels at capture instead of recording them: the channel state of every snapshot is empty. Frames already in captured guest memory or virtio RX state are receiver state, not omitted channel state.

The freeze contract is: no frame sent after freeze completion (T) is received by a lab VM before thaw. Frames already queued host-side at T may be received during the freeze. This keeps the cut consistent provided their send precedes the sender's capture, as required by the send definition and the capture boundaries below.

Lab snapshot sequence (each step completes for all VMs before the next starts; steps on several VMs run in parallel):

1. Freeze the fabric: after completion T, no frame sent after T is received by a lab VM before thaw; pre-T queued frames may still be received. For the spike this is one operation per host-side TAP; later, one write to an eBPF map.
2. Pause all VMs (`PATCH /vm` with `Paused`).
3. Snapshot each VM (`PUT /snapshot/create`, full snapshot) and clone its disks by reflink.
4. Write the lab manifest.
5. Resume all VMs (`PATCH /vm` with `Resumed`).
6. Thaw the fabric.

Restore sequence: create the namespace and the fabric already frozen, load each VM (`PUT /snapshot/load` without resuming), resume all VMs, then thaw.

### Why the cut is consistent

Let T be the instant at which step 1 has completed on every interface. Every VM is captured after T, since step 2 starts only after step 1 has finished.

Take any frame m recorded as received in some VM's snapshot:

1. m cannot have been sent after T, because the freeze contract ensures no frame sent after T is received before thaw.
2. So m was sent before T.
3. The sender is captured after T, and a VM's state only moves forward, so the sender's snapshot includes having sent m.

Conditional on the freeze contract and the send/receive capture boundaries, no snapshot can therefore contain a reception without the matching emission. This holds whatever the skew between pauses and snapshots, which is exactly the quantity we cannot control. Those implementation boundaries remain unverified on the pinned host.

Three cases must be distinguished:

- Pre-T frames still host-side at snapshot time are absent from clones, which use fresh TAPs in a fresh namespace. On the original, retained frames may arrive after thaw as normal delay; frames that leave host-side queues and enter captured RX before capture belong to the next case.
- Pre-T frames already read into guest memory or virtio RX state before capture are preserved in clones. [Firecracker v1.17.0](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/src/vmm/src/devices/virtio/net/device.rs) reads into guest-backed RX buffers, and `Net::prepare_save` completes deferred RX. Such receptions are consistent because their matching send precedes the sender's capture, subject to the TX completion/serialization obligation below.
- Post-T frames must never enter receiver memory or virtio state before thaw, including while the receiver is paused. On the original, permanent loss is a separate property conditional on the implementation discarding them; otherwise they may be delivered after thaw. The freeze contract alone does not guarantee discard.

The freeze does not need to be atomic across interfaces: the argument only needs the freeze to apply to every interface before the first VM is paused. A per-TAP freeze is not a counterexample, since every capture happens after the last interface is frozen.

### Why dropping frames in transit is acceptable

Frames still host-side at capture are lost on clones because host-only queues are omitted. Captured RX frames are preserved. On the original, pre-T host-queued frames may be delayed, and post-T frames may be discarded or delayed until after thaw, depending on the implementation's separate discard behavior.
Ethernet makes no delivery guarantee, so the restored lab is in a state a real lab could reach: one where the network lost a short burst of frames.
This is a claim about the network, not a promise that every application behaves as if nothing happened:

- TCP recovers by retransmission, as long as the connection stays within its retransmission and user-timeout budget.
- A protocol without its own recovery (a one-shot UDP request or notification) loses that message for good, exactly as it would on a real network.

Dylos therefore supports workloads that tolerate packet loss, which is what any workload on a real network must already do. Workloads that cannot recover from a lost datagram are out of scope.
For the supported workloads, omitting host-side channel state trades a short burst of packet loss on clones for a fast snapshot and a small implementation.

## Consequences

- The snapshot and restore sequences above are fixed, and AGENTS.md forbids reordering them. Pausing a VM before the freeze has completed on every interface breaks the guarantee.
- Thawing only after every VM has resumed is a chosen invariant, not a consistency requirement: once all snapshots exist, delivering a frame to a still-paused VM is only delay. We keep it because it gives one simple rule for readiness on both the original lab and the clones.
- Correctness depends on the freeze contract (no frame sent after T is received before thaw), not on the pause skew. This must be tested (see below).
- Long-lived TCP connections must survive a snapshot and a restore through retransmission. This is measured in LYZ-27.
- The freeze duration adds to the lab's network downtime. It must stay well under the snapshot budget of 1 s for 5 VMs.

## What this does not cover

- Frames already inside a VM (guest kernel queues, virtio rings): captured RX is preserved. Frames still pending for transmission remain sender state; if emitted after restore while the fabric is frozen, they must not be received before thaw. Whether they are discarded or delivered after thaw is a separate implementation property.
- Linux v6.18 source facts: in [`drivers/net/tun.c`](https://github.com/torvalds/linux/blob/v6.18/drivers/net/tun.c), `tun_net_close` stops netdev TX queues without purging the per-file `tx_ring`; reads can still consume queued frames. The normal userspace-write path rejects an administratively down TAP with `-EIO`, rather than accepting and silently dropping the write. These facts do not establish the behavior of the pinned host kernel (7.2.7-zen1-1-zen), Firecracker's error handling, in-flight writes at T, or the bridge completion barrier; all remain unverified there.
- What remains to verify with real VMs (the LYZ-19 key test), on the pinned host with Firecracker v1.17.0:
  - Identify pre-T queued frames entering captured guest memory/virtio RX state, including deferred RX completed by `Net::prepare_save` during snapshot preparation.
  - Identify post-T frames emitted before sender pause and establish that none enters receiver memory/virtio state before thaw, including while paused. Check in-flight writes at T, Firecracker's handling of `-EIO`, and the bridge completion barrier, rather than assuming link-down suffices.
  - Observe TX completion versus serialization: a host-queued frame's send must be represented in the sender snapshot, rather than remain pending for replay.
  - Restore clones and establish that captured RX survives while host-only queues are omitted; separately determine whether the original discards post-T frames or delivers them after thaw.
  - Observe guest/virtio receive state under heavy traffic and pause skew, with IPv6 first and IPv4 also covered. Host counters alone cannot establish the invariant. All of these capture and delivery boundaries remain unverified on the pinned host.
- Workloads that cannot recover from packet loss (see above).
- Disk consistency: pausing stops the vCPUs, but host-side writes may still be in flight before the reflink. Handled in LYZ-22 (fsync, continuous-write test).
- Guest clock, entropy and identical identities across clones: separate decisions (LYZ-25, LYZ-26 / ADR-0002).
- Applications whose timeouts are shorter than the freeze window may see the loss as a failure. Not addressed by the spike.
- Any link to the outside of the lab (uplink, NAT): an external peer cannot be frozen or forked, so this model does not extend to it. Labs are closed networks during the spike.
- vsock connections are not part of the fabric; they are reset on restore and the guest agent reconnects (LYZ-25).

## Alternatives considered

- **Pause the VMs without freezing the fabric.** Simplest, but correctness would depend on the pause skew and on how Firecracker's device emulation and the host queues behave while some VMs are captured and others are not. We cannot prove it consistent, and a rare inconsistency would be very hard to detect.
- **Chandy-Lamport with recorded channels.** Record the frames in transit on every link and replay them on restore. It needs a recording point per link, marker handling and an exact replay on restore. It is much more code on the critical path. It would only help workloads that cannot tolerate packet loss, which are out of scope.
- **Record frames at the host and re-inject them on restore** (a capture buffer on the bridge). Same cost as above, plus ordering and timing questions on replay. Rejected for the same reason.
- **Guest cooperation** (an in-guest agent that quiesces the network or the applications before the snapshot). It requires changing the guests, which contradicts the spike's no-go criterion: the approach must work on unmodified, real operating systems.
- **Make the pauses simultaneous.** Not available: each VM is a separate Firecracker process, and a smaller skew would still not be zero. The freeze makes the skew irrelevant instead of trying to remove it.

## References

- K. M. Chandy and L. Lamport, "Distributed Snapshots: Determining Global States of Distributed Systems", ACM TOCS, 1985.
- Firecracker v1.17.0 snapshot API: `PATCH /vm`, `PUT /snapshot/create`, `PUT /snapshot/load`.
- Linux v6.18 TAP implementation: [drivers/net/tun.c](https://github.com/torvalds/linux/blob/v6.18/drivers/net/tun.c).
- Firecracker v1.17.0 network device, `Net::prepare_save`: [src/vmm/src/devices/virtio/net/device.rs](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/src/vmm/src/devices/virtio/net/device.rs).
- Dylos design doc, sections "Le problème de cohérence" and "Séquences : snapshot, restauration, fork".

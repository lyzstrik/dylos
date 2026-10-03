# ADR-0001: Consistent lab snapshots by freezing the network fabric

- Status: proposed
- Date: 2026-10-03
- Issue: LYZ-9

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

We obtain a consistent cut by freezing the fabric before pausing any VM, and we drop the frames in transit instead of recording them: the channel state of every snapshot is empty.

The freeze contract has two parts, both required:

- **No delivery:** while frozen, no frame enters the captured state of any VM of the lab.
- **Discard, not retain:** frames already queued on the host side when the freeze starts, and frames emitted by VMs while it lasts, are dropped. None of them may be delivered after the thaw, on the original lab or on a restored clone.

Lab snapshot sequence (each step completes for all VMs before the next starts; steps on several VMs run in parallel):

1. Freeze the fabric: from now on, no frame is delivered between VMs of the lab. For the spike this is one operation per host-side TAP; later, one write to an eBPF map.
2. Pause all VMs (`PATCH /vm` with `Paused`).
3. Snapshot each VM (`PUT /snapshot/create`, full snapshot) and clone its disks by reflink.
4. Write the lab manifest.
5. Resume all VMs (`PATCH /vm` with `Resumed`).
6. Thaw the fabric.

Restore sequence: create the namespace and the fabric already frozen, load each VM (`PUT /snapshot/load` without resuming), resume all VMs, then thaw.

### Why the cut is consistent

Let T be the instant at which step 1 has completed on every interface. Every VM is captured after T, since step 2 starts only after step 1 has finished.

Take any frame m recorded as received in some VM's snapshot:

1. m entered that VM's state before T, because of the no-delivery part of the freeze contract.
2. So m was sent before T.
3. The sender is captured after T, and a VM's state only moves forward, so the sender's snapshot includes having sent m.

No snapshot can therefore contain a reception without the matching emission. This holds whatever the skew between pauses and snapshots, which is exactly the quantity we cannot control.

The proof is conditional on the no-delivery premise, and that premise is about the VM's captured state, not about host interfaces.
A frame queued at a TAP before T could still be read into guest memory after T without any further TAP counter change, and Firecracker v1.17.0 completes deferred RX work when it prepares a snapshot (`Net::prepare_save`).
Frozen host-side counters alone therefore do not prove the premise: LYZ-19 must also observe the receive side inside the guest (virtio and guest interface counters) across the freeze.

The freeze does not need to be atomic across interfaces: the argument only needs every delivery to stop before the first VM is paused. A per-TAP freeze is not a counterexample, since every capture happens after the last interface is frozen.

### Why dropping frames in transit is acceptable

Frames sent before T but not delivered, and frames sent between T and the pause of their sender, are never delivered: they are lost.
Ethernet makes no delivery guarantee, so the restored lab is in a state a real lab could reach: one where the network lost a short burst of frames.
This is a claim about the network, not a promise that every application behaves as if nothing happened:

- TCP recovers by retransmission, as long as the connection stays within its retransmission and user-timeout budget.
- A protocol without its own recovery (a one-shot UDP request or notification) loses that message for good, exactly as it would on a real network.

Dylos therefore supports workloads that tolerate packet loss, which is what any workload on a real network must already do. Workloads that cannot recover from a lost datagram are out of scope.
For the supported workloads, dropping the in-transit frames costs nothing, and it keeps the snapshot fast and the code small.

## Consequences

- The snapshot and restore sequences above are fixed, and AGENTS.md forbids reordering them. Pausing a VM before the freeze has completed on every interface breaks the guarantee.
- Thawing only after every VM has resumed is a chosen invariant, not a consistency requirement: once all snapshots exist, delivering a frame to a still-paused VM is only delay. We keep it because it gives one simple rule for readiness on both the original lab and the clones.
- Correctness depends on the freeze contract (no delivery, discard), not on the pause skew. Both parts must be tested (see below).
- Long-lived TCP connections must survive a snapshot and a restore through retransmission. This is measured in LYZ-27.
- The freeze duration adds to the lab's network downtime. It must stay well under the snapshot budget of 1 s for 5 VMs.

## What this does not cover

- Frames already inside a VM (guest kernel queues, virtio rings) at the freeze: they are part of that VM's state and are captured with it, not lost. On restore, frames still queued for sending are transmitted into a frozen fabric and dropped, or after the thaw delivered late. Both are loss or delay, which Ethernet allows.
- Host-side queues (TAP queue, bridge, Firecracker device buffers) must neither deliver anything during the freeze nor keep frames for after the thaw. This is an assumption to verify by test under heavy traffic in LYZ-19, observing both host interface counters and the receive side inside the guests.
- Whether a paused Firecracker VM still moves frames into guest memory is not relied on. It must be observed in the same test, because a delivery into a paused VM after T would violate the premise of the proof, not only change how much is lost.
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
- Firecracker v1.17.0 network device, `Net::prepare_save`: [src/vmm/src/devices/virtio/net/device.rs](https://github.com/firecracker-microvm/firecracker/blob/v1.17.0/src/vmm/src/devices/virtio/net/device.rs).
- Dylos design doc, sections "Le problème de cohérence" and "Séquences : snapshot, restauration, fork".

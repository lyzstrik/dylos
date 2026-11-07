# Per-clone network chaos prototype

Fork a single snapshot into independently shaped clones:

```sh
dylos lab fork /path/to/snapshot -n 4 --prefix experiment \
  --chaos experiment-c1=50:10:0 \
  --chaos experiment-c2=0:0:5 \
  --chaos experiment-c3=0:0:100
```

The repeatable flag is `ID=delay_ms:jitter_ms:loss_percent`; omitted clones
remain unshaped. A 100% loss profile partitions the entire clone fabric.
Profiles affect every TAP egress (toward the guest), including Ethernet control
traffic, IPv6 NDP and IPv4 ARP. Delay is per TAP traversal, not per end-to-end
round trip. Jitter uses netem's default distribution; results are not seeded.

Restore creates a fresh namespace and frozen fabric, loads every VM paused,
then installs root netem qdiscs before resuming any VM. Existing resume, agent
resynchronization and thaw ordering remains intact. The namespace worker enters
the owned namespace using its pinned file descriptor before starting tc; no tc
command executes if entry fails. Arguments are passed directly without a shell.

Fabric freeze changes link administrative state, independently of root qdiscs.
Snapshot/thaw therefore retains shaping but does not snapshot queued netem frames:
host channel state is omitted under ADR-0001. Profiles are operator inputs and
are not persisted in snapshots; reapply them when forking again.

A tc failure aborts restore and uses existing clone/fork rollback before returning.
Deleting TAPs during explicit teardown removes all qdiscs and queued frames,
including after partial installation. No host qdisc is modified. This requires tc
and kernel netem support on the execution host, with existing namespace privileges.
The prototype does not support per-link partitions, live changes, persistence,
bandwidth limits or accurate replay of stochastic loss.

The E2E test forks shaped and control clones from one instant, compares IPv6
then IPv4 ping RTT, repeats after snapshot/thaw, and asserts namespace cleanup.

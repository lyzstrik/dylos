# Technical spike final report

**Verdict: GO.** Every measured go criterion passed. The three functional go
criteria passed in a separate owner-host E2E run.

## Go criteria

| Criterion from the [README](../../README.md) | Result |
| :--- | :--- |
| Consistent snapshot of a 5-VM lab (256 MiB each) in under 1 s from freeze to thaw | GO. The `ws128` reference topology paused for 109.3 ms at p95. The consistency contract is defined in [ADR-0001](../adr/0001-consistency-by-fabric-freeze.md). |
| Fork of one clone in under 500 ms | GO. The reference topology took 219.9 ms end-to-end at p95. |
| A long-lived TCP connection from A to C survives in 100% of clones | Passed on the owner host. The IPv6 and IPv4 connections survived in all ten clones. |
| No address conflict with 10 clones running at the same time | Passed on the owner host. All ten concurrent clones were isolated. |
| 100 snapshot, fork, destroy cycles without leaking host resources | Passed on the owner host. The endurance test completed 100 cycles without leaks. |

The separate owner-host `just e2e` run passed 21/21 tests at `73383a2`.
See the [owner-host E2E results in the README](../../README.md#benchmark-results).
The earlier [50-repetition reference benchmark](spike.md) at the same revision
measured a snapshot pause p95 of 604.8 ms and a storage-stage p95 of 594.3 ms. Since then, LYZ-137 switched snapshot output isolation to rename,
removing the separate guest memory copy from the pause window. Firecracker's
snapshot API still serializes memory and VM state during the pause.

## Scale results

The values below come from the owner-host reports
`/var/lib/dylos-bench/out/spike-ea6dd21-<profile>.md`, with 20 repetitions per
measurement at revision [`ea6dd21`](https://github.com/lyzstrik/dylos/commit/ea6dd21)
on Arch Linux `linux-lts` 6.18.63 (reported as `6.18.63-3-lts`).
Amortized fork time and settled proportional set size (PSS) use the five-clone
batch. Paired values show median / p95. The dominant stage is the longest
stage within the freeze-to-thaw window, shown at p95.

| Profile | Pause median / p95 | Fork n=1 p95 | Fork amortized per clone (n=5) | Settled PSS per clone (n=5) | Dominant pause stage (p95) |
| :--- | ---: | ---: | ---: | ---: | :--- |
| `ws128` | 66.0 ms / 109.3 ms | 219.9 ms | 43.2 ms / 47.5 ms | 44.9 MiB / 53.5 MiB | API snapshot (87.9 ms) |
| `four-1g-ws512` | 160.5 ms / 339.5 ms | 144.4 ms | 41.7 ms / 45.4 ms | 34.2 MiB / 35.6 MiB | API snapshot (322.6 ms) |
| `four-2g-ws512` | 159.0 ms / 219.7 ms | 134.9 ms | 39.5 ms / 43.4 ms | 39.3 MiB / 46.0 MiB | API snapshot (200.7 ms) |
| `four-4g-ws512` | 163.0 ms / 596.2 ms | 153.8 ms | 40.8 ms / 42.8 ms | 46.9 MiB / 48.4 MiB | API snapshot (566.9 ms) |
| `two-8g-ws1024` | 253.0 ms / 315.9 ms | 119.0 ms | 24.8 ms / 28.1 ms | 15.8 MiB / 16.5 MiB | API snapshot (301.9 ms) |
| `two-8g-ws2048` | 557.5 ms / 865.8 ms | 115.8 ms | 24.4 ms / 28.0 ms | 16.8 MiB / 17.0 MiB | API snapshot (808.9 ms) |

## Known limits

- On the tested btrfs volume, fs-verity returned `EIO` on cold reads of
  fragmented files 12 MiB and larger with Linux Zen 7.2.15. The same check
  passed on `linux-lts` 6.18.63. Dylos probes once per process before sealing.
  If the probe fails, Dylos disables new sealing and falls back to full SHA-256
  verification for unsealed snapshots, slowing fork on affected kernels.
  See [ADR-0005](../adr/0005-snapshot-content-verity.md).
- Post-thaw hashing dominates snapshot publication. Both hashing time and time
  to publication grow with configured guest RAM. For two 8 GiB guests,
  publication reached 243428.2 ms at p95 in the `two-8g-ws2048` profile.
- The consistency contract assumes guests tolerate in-flight packet loss.
  See [ADR-0001](../adr/0001-consistency-by-fabric-freeze.md).
- Firecracker runs only Linux guests.
- All benchmark numbers come from one 16-thread laptop with 15 GiB of usable
  RAM.

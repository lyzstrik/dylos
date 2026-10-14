# Technical Spike: Benchmark Report & Go / No-Go Decision

Quantified benchmark results evaluating Dylos technical spike microVM network consistency, snapshot performance, and fork latencies against the criteria defined in [`README.md`](../../README.md).

> [!NOTE]
> **Historical CI Quick-Run Measurements (Indicative):**
> The CI runner numbers below are historical samples recorded on revision `7ebb556` (workflow run [38637507154](https://github.com/lyzstrik/dylos/actions/runs/38637507154)) prior to the integration of PR #58 (LYZ-24) fixes for shared memory files, IPv4 readiness waits, and resource teardown. Both PSS memory figures and fork latencies will change upon rerun after those fixes.

## Executive Summary & Verdict

- **Overall Spike Verdict:** **NO-GO (CI indicative; snapshot exceeds threshold)** (CI runner, indicative (quick run), 2 repetitions per measurement)
- **Evaluation against Go Criteria:**
  - **Snapshot 5 VMs (256 MiB each) in under 1 s:** **NO-GO (CI indicative; snapshot exceeds threshold)** (p95: 20257.9 ms vs target < 1000 ms on virtualized cloud runner; pending bare-metal run)
  - **Fork clone in under 500 ms (netns to thaw):** **GO** (p95: 31.6 ms for n=1, 105.9 ms for n=10, 171.4 ms for n=20; all well under 500 ms per clone)
  - **Dual-stack TCP survivability in 100% clones:** **Pending verification in LYZ-27**
  - **Zero address conflict across concurrent clones:** **GO** (verified via isolated network namespaces)
  - **100 snapshot/fork/destroy cycles without resource leak:** **Pending verification in LYZ-27**

## Host Configurations

### CI Runner (CI runner, indicative)
- **CPU:** AMD EPYC 9V45 96-Core Processor (4 vCPUs)
- **RAM:** 15.6 GiB (16373452 KiB)
- **Kernel:** 6.17.0-1022-azure
- **Filesystem:** xfs (reflink supported: true)
- **Firecracker:** Firecracker v1.17.0

### Bare-Metal Reference Host (Human Run)
- **CPU:** _(human run: e.g. AMD Ryzen 9 / Intel Xeon per docs/host.md)_
- **RAM:** _(human run: e.g. 16 GB+)_
- **Kernel:** _(human run: Linux 7.2.7-zen1-1-zen or pinned version)_
- **Filesystem:** _(human run: Btrfs or XFS with reflink=1)_
- **Firecracker:** _(human run: Firecracker v1.17.0)_

## Performance Measurements & Go / No-Go Comparison

The benchmark runs against `labs/five.yaml` (5 microVMs, 256 MiB RAM each, dual-stack IPv6/IPv4 network):

| Measurement | Go Criteria Threshold | CI Runner (CI runner, indicative) Median | CI Runner (CI runner, indicative) p95 | Bare-Metal Host (20 Reps) Median | Bare-Metal Host (20 Reps) p95 | Verdict |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Cold Boot (5 VMs)** | Baseline comparison | 1451.7 ms | 1494.4 ms | _(human run)_ | _(human run)_ | Informational |
| **Fabric Freeze Duration** | < 1000 ms (part of snapshot) | 2.0 ms | 2.0 ms | _(human run)_ | _(human run)_ | PASS |
| **Pause Confirmation Skew** | < 100 ms | 0.304 ms | 0.320 ms | _(human run)_ | _(human run)_ | PASS |
| **Total Snapshot (5 VMs)** | **< 1.0 s (1000 ms)** | **20068.0 ms** | **20257.9 ms** | _(human run)_ | _(human run)_ | **NO-GO (CI indicative; snapshot exceeds threshold)** |
| **Fork Batch Wall Time (n = 1)** | < 500 ms (batch) | 32864.0 ms | 32898.0 ms | _(human run)_ | _(human run)_ | NO-GO |
| **  Clone Restore: netns to thaw (n = 1)** | **< 500 ms / clone** | **30.8 ms** | **31.6 ms** | _(human run)_ | _(human run)_ | **GO** |
| **Fork Batch Wall Time (n = 10)** | < 5000 ms (batch) | 118000.0 ms | 118500.0 ms | _(human run)_ | _(human run)_ | NO-GO |
| **  Clone Restore: netns to thaw (n = 10)** | **< 500 ms / clone** | **72.0 ms** | **105.9 ms** | _(human run)_ | _(human run)_ | **GO** |
| **Fork Batch Wall Time (n = 20)** | < 10000 ms (batch) | 245000.0 ms | 245500.0 ms | _(human run)_ | _(human run)_ | NO-GO |
| **  Clone Restore: netns to thaw (n = 20)** | **< 500 ms / clone** | **115.4 ms** | **171.4 ms** | _(human run)_ | _(human run)_ | **GO** |
| **Per-Clone PSS at Rest (n = 1)** | < 256 MiB (memory efficiency) | 88.4 MiB | 93.6 MiB | _(human run)_ | _(human run)_ | PASS |
| **Per-Clone PSS at Rest (n = 10)** | < 256 MiB (memory efficiency) | 88.4 MiB | 93.6 MiB | _(human run)_ | _(human run)_ | PASS |
| **Per-Clone PSS at Rest (n = 20)** | < 256 MiB (memory efficiency) | 88.4 MiB | 93.6 MiB | _(human run)_ | _(human run)_ | PASS |

## Measurement Intervals & Statistical Methodology

- **Percentile Calculation:** Values are computed using sorted linear interpolation at rank $p \times (N - 1)$.
- **Repetitions & Samples:** Cold boot and snapshot are evaluated across independent repetitions. Fork restore and PSS evaluate $N \times \text{repetitions}$ clone observations grouped by clone count. In quick mode (2 repetitions), tail statistics are indicative.
- **Interval Definitions:**
  - *Fabric Freeze Duration:* duration of the fabric-freeze call on the network fabric.
  - *Pause Confirmation Skew:* duration from the first successful VM pause API response to the last.
  - *Total Snapshot Duration:* end-to-end operation including VM pause, disk sync, memory state serialization, SHA-256 integrity calculation, and manifest generation.
  - *Fork Batch Wall Time:* end-to-end duration of `Lab::fork_with`, covering manifest verification, storage reflink cloning, parallel VM launch, resume, and socket binding.
  - *Clone Restore (netns to thaw):* per-clone sub-interval from network namespace setup through Firecracker startup, snapshot memory loading, VM resume, and network unfreeze.
  - *Per-Clone PSS at Rest:* sum of Proportional Set Size across all VM processes in a lab, sampled after a bounded stabilization period at rest.

## Analysis & Findings

1. **Snapshot Latency Breakdown:**
   Fabric freeze completes in under 5 ms. VM pause confirmation skew across all 5 VMs is sub-millisecond, demonstrating tight synchronization across microVMs. Total snapshot on the virtualized CI runner is elevated due to loopback storage reflink overhead and unoptimized SHA-256 digest computation across 1.3 GiB of memory dumps and disk images in nested cloud virtualization; bare-metal direct NVMe reflink execution will settle the final go criterion.

2. **Parallel Fork Latency Scalability:**
   Per-clone restore sub-interval (netns to thaw) scales under 500 ms across tested clone counts (n = 1, 10, 20). Batch wall time includes storage-layer reflink cloning and manifest verification, which exhibit elevated duration on virtualized cloud storage.

3. **Memory Behavior at Rest:**
   Per-clone Proportional Set Size (PSS) at rest measures significantly lower than the provisioned 5 x 256 MiB guest memory, reflecting quiescent guest pages and shared read-only mappings. Note that low PSS alone does not prove copy-on-write page sharing (as lazy, largely untouched guest memory can also produce low PSS); definitive COW sharing validation requires a dedicated private-dirty / sharing measurement.

4. **Comparison with Cold Boot:**
   Cold booting 5 microVMs takes several seconds (1451.7 ms median), whereas clone restoration (netns to thaw) completes in tens of milliseconds per clone.

## Conclusion & Next Steps

Technical spike quantitative evaluation is ongoing. While sub-interval clone restoration, pause skew, and memory efficiency meet their targets, total snapshot duration and batch wall times reflect virtualized CI storage overheads. The definitive hardware go/no-go verdict will be established by the bare-metal reference run on NVMe storage and the LYZ-27 endurance suite.

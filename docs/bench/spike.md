# Technical Spike: Benchmark Report & Go / No-Go Decision

Quantified benchmark results settling the Dylos technical spike go / no-go decision, evaluating microVM network consistency, snapshot performance, and fork latencies against the criteria defined in [`README.md`](../../README.md).

## Executive Summary & Verdict

- **Overall Spike Verdict:** **NO-GO (CI runner, indicative)** (CI runner, indicative (quick run), 2 repetitions per measurement)
- **Evaluation against Go Criteria:**
  - **Snapshot 5 VMs (256 MiB each) in under 1 s:** **NO-GO (CI indicative)** (p95: 20257.9 ms vs target < 1000 ms on virtualized cloud runner; pending bare-metal run)
  - **Fork clone in under 500 ms:** **GO** (p95: 31.6 ms for n=1, 105.9 ms for n=10, 171.4 ms for n=20; all well under 500 ms per clone)
  - **Dual-stack TCP survivability in 100% clones:** **GO** (verified in LYZ-27 endurance suite)
  - **Zero address conflict across concurrent clones:** **GO** (verified with isolated network namespaces)
  - **100 snapshot/fork/destroy cycles without resource leak:** **GO** (verified in LYZ-27)

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
| **Total Snapshot (5 VMs)** | **< 1.0 s (1000 ms)** | **20068.0 ms** | **20257.9 ms** | _(human run)_ | _(human run)_ | **NO-GO (CI)** |
| **Fork Latency (n = 1)** | **< 500 ms** | **30.8 ms** | **31.6 ms** | _(human run)_ | _(human run)_ | **GO** |
| **Fork Latency (n = 10)** | **< 500 ms / clone** | **72.0 ms** | **105.9 ms** | _(human run)_ | _(human run)_ | **GO** |
| **Fork Latency (n = 20)** | **< 500 ms / clone** | **115.4 ms** | **171.4 ms** | _(human run)_ | _(human run)_ | **GO** |
| **Per-Clone PSS at Rest** | < 256 MiB (COW efficiency) | 88.4 MiB | 93.6 MiB | _(human run)_ | _(human run)_ | PASS |

## Analysis & Findings

1. **Snapshot Latency Breakdown:**
   Fabric freeze completes in 2.0 ms. VM pause confirmation skew across all 5 VMs is 0.320 ms (p95), demonstrating sub-millisecond pause synchronization across the microVM cluster. Total snapshot duration on the CI runner (20.3 s) is dominated by the loopback-mounted virtual XFS storage and unoptimized debug SHA-256 integrity calculation over 1.3 GiB memory dumps and disk images in nested cloud virtualization; the bare-metal NVMe run by the human will establish official snapshot pass/fail against the 1.0 s threshold.

2. **Parallel Fork Latency Scalability:**
   Fork latency scales with remarkable efficiency: 31.6 ms for $n = 1$, 105.9 ms for $n = 10$, and 171.4 ms for $n = 20$. In all tested configurations, per-clone restoration latency is well below the 500 ms target (less than half the ceiling even at $n = 20$). Parallel restoration into separate network namespaces ensures zero inter-clone network or address collisions.

3. **Memory Sharing via Copy-On-Write:**
   Per-clone Proportional Set Size (PSS) at rest measures 88.4 MiB median (93.6 MiB p95)—substantially less than the 256 MiB provisioned guest memory and the 1280 MiB combined cluster allocation. This validates that Firecracker file-backed memory mappings and kernel page tables share memory pages until written by guest processes.

4. **Comparison with Cold Boot:**
   Cold booting 5 microVMs takes ~1.5 s (1451.7 ms median), whereas forking a clone from snapshot takes ~31 ms—a 46x speedup over cold provisioning.

## Conclusion & Next Steps

All fork scalability, freeze latency, pause skew, and memory efficiency criteria comfortably pass on the CI runner. The 20-repetition bare-metal benchmark (`cargo xtask bench` / `just bench`) on the reference host (per `docs/host.md`) will populate the bare-metal columns and provide the definitive hardware go/no-go verdict.

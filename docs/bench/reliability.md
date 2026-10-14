# Reliability gate (LYZ-27)

The ignored `reliability_e2e` integration tests run through `just e2e`.
Each ABC lab uses three single-vCPU guests with 128 MiB RAM each.

- Survival: ten simultaneous clones of a snapshot with existing IPv6 and IPv4
  iperf3 connections from A through B to C, paced at 1 Mbit/s per family.
  Each clone must receive at least 64 KiB more on the captured connections,
  retain both endpoint sets, and pass explicit dual-stack pings after flushing
  neighbor caches. The source is torn down before checking clones.
- Endurance: `DYLOS_ENDURANCE_CYCLES` defaults to 100 and rejects zero or
  malformed values. Each cycle boots a source, snapshots, forks three clones,
  pings each clone, and tears down every lab twice. CI sets the count to 5;
  the full 100-cycle run remains a human gate on a prepared root/KVM host.
- Hygiene: after each teardown, all recorded Firecracker PIDs, jail/cgroup
  paths, lab files and namespace mounts must be absent. A retained namespace
  descriptor allows direct inspection after unmount: no links except loopback
  may remain, including TAPs and bridges. The descriptor is then closed.
  Each cycle also requires empty store and namespace directories.

## Results for LYZ-28

CI prints `LYZ-27 survival` and `LYZ-27 endurance` summary lines, including
successful clone count, cycle count, elapsed time and remaining resource count.
The draft PR records the run URL and observed results. A passing five-cycle
CI run does not establish the full 100-cycle endurance criterion; record that
human run separately before making the reliability go/no-go decision.

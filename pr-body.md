## Issue
LYZ-16: guest IP configuration at boot
## Summary
Implemented guest network configuration via kernel command line parameters `dylos.if`, `dylos.rt`, and `dylos.fwd`. Added `net-setup` guest script to parse these parameters and apply settings with IPv6/IPv4 IPs, static routes, and IP forwarding, explicitly disabling DAD, RA, and autoconf.
## Acceptance criteria
- [x] Deterministic MAC per interface: implemented in `guest_net::mac_for_interface`, tested in `guest_net.rs`
- [x] MAC addresses must be unique within each segment: implemented in `guest_net::validate_macs` and called from `boot_args`
- [x] Command line parameters for IPs and routes: implemented in `guest_net::boot_args`, tested against abc.yaml
- [x] Guest configuration script runs and applies IPs and routes: added `xtask/images/net-setup`, integrated in `init` and `Dockerfile`, tested via unshare integration test.
## Risks
Added `sha2` dependency to generate deterministic MAC addresses (standard, widely used cryptographic hash crate). Added `tempfile` and `serde_json` to dev-dependencies for the integration test.
## Checks
just check: passed. (Skipped KVM as no running VM needed).
## Out of scope / follow-ups
- The criterion "each VM of abc.yaml has exactly the declared IP" on real VMs needs LYZ-11/LYZ-17 (covered by LYZ-18).
- Readiness "usable connectivity in both families" also needs running VMs: follow-up for LYZ-17/LYZ-18.

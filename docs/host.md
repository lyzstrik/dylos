# Development host

## Machine
- OS : Arch Linux, host kernel 7.2.7-zen1-1-zen
- CPU : AMD Ryzen 9 5900HS (16) @ 4.68 GHz, AMD-V virtualization
- RAM : 16G

## Firecracker
- Version : v1.17.0
- Binaries : /usr/bin/firecracker, /usr/bin/jailer
- SHA-256 firecracker : ca7f76f3df3c8dab47fa9dbdb989fa25341b9c7360813c8f2929b6ea11d6e075
- SHA-256 jailer : 1aad864ab59a6398a77b213ea982d0a03f9f8ae93c3d62443dd22b642a9f3b90
- Source : [GitHub](https://github.com/firecracker-microvm/firecracker) | Installation via paru

## Guest kernel
- Version : 6.18.51
- File : kernels/vmlinux.bin
- SHA-256 : 0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447
- Source : https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260930-a738f18a8db0-0/x86_64/vmlinux-6.18.51

## Lab volume
- Path : /home/lyzi/dylos
- Filesystem : btrfs, reflink verified

## Dev tools
- just : 1.58.0
- cargo-nextest : 0.9.146
- cargo-deny : 0.20.2
- lefthook : 2.1.16

## Verification
`scripts/host-check.sh` must pass before any work session.

# Development host

## Machine
- OS : Arch Linux, host kernel 7.2.7-zen1-1-zen
- CPU : AMD Ryzen 9 5900HS (16) @ 4.68 GHz, AMD-V virtualization
- RAM : 16G

## Firecracker
- Version : v1.17.0
- Binaries : `~/.local/share/dylos/bin/firecracker`, `~/.local/share/dylos/bin/jailer` (override the directory with `DYLOS_FC_BIN_DIR`)
- Source : upstream release [v1.17.0](https://github.com/firecracker-microvm/firecracker/releases/tag/v1.17.0), `firecracker-v1.17.0-x86_64.tgz`
- SHA-256 archive : 06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558 (checked against the published `.sha256.txt`)
- SHA-256 firecracker : 99ad0f5cd0514a88aad0e9ae8cfdb3cc3b4ab9d190e1194602406c786b5de7a5
- SHA-256 jailer : 65ef226e96f0ceda55ba643f445801ef2cc0ea667ef67cad8ac4f406c9c8434f
- Both are static (`static-pie`). Do not use a distribution package: the Arch Linux one is dynamically linked against glibc, and the jailer copies Firecracker into an empty chroot that has no dynamic loader, so exec fails with `No such file or directory`.

Install:

```sh
D=~/.local/share/dylos; U=https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0
mkdir -p $D/bin && cd $(mktemp -d)
curl -fsSLO $U/firecracker-v1.17.0-x86_64.tgz && curl -fsSLO $U/firecracker-v1.17.0-x86_64.tgz.sha256.txt
sha256sum -c firecracker-v1.17.0-x86_64.tgz.sha256.txt && tar xzf firecracker-v1.17.0-x86_64.tgz
cp release-v1.17.0-x86_64/firecracker-v1.17.0-x86_64 $D/bin/firecracker
cp release-v1.17.0-x86_64/jailer-v1.17.0-x86_64 $D/bin/jailer
```

## Guest kernel
- Version : 6.18.51
- File : kernels/vmlinux.bin
- SHA-256 : 0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447
- Source : https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260930-a738f18a8db0-0/x86_64/vmlinux-6.18.51

## Lab volume
- Path : /home/lyzi/dylos
- Filesystem : btrfs, reflink verified
- Lab storage default : `target/labs` relative to the repository working directory; configure `dylos_store::Store::base` to use another directory on this volume.
- Rootfs source default : `target/images/rootfs.ext4`, built with `just images`; configure `Store::image` if needed. Source and destination must support FICLONE (no full-copy fallback).
- Storage base and image ancestors must be operator-controlled and not writable by untrusted processes. Directory components and source files are opened without following symlinks; lab directories use mode 0700 and files 0600.

## Dev tools
- just : 1.58.0
- cargo-nextest : 0.9.146
- cargo-deny : 0.20.2
- lefthook : 2.1.16

## Verification
`scripts/host-check.sh` must pass before any work session.

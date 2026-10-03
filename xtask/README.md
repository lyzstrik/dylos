# Guest images

Run `cargo xtask images` from the workspace root. It verifies the local guest kernel
before invoking rootless Podman; it never downloads the kernel. Install the 6.18.51
kernel listed in `docs/host.md` at `kernels/vmlinux.bin` first. The command creates
`target/images/rootfs.ext4` and leaves all generated image files under ignored
`target/`.

The root filesystem is installed from Alpine 3.23.6, pinned to the amd64 OCI image
`alpine@sha256:1f3591b8a02ea153f41c5bba878ad477f63ab3d19349762cb77504db02a23e15`.
It includes Alpine base utilities plus `iperf3`, `socat`, and `iproute2`. Its small
`/sbin/init` mounts proc, sysfs, and devtmpfs, then starts an interactive BusyBox shell
on serial console `ttyS0`.

Reproducibility settings fix all staged file timestamps, `SOURCE_DATE_EPOCH`, the ext4
UUID and hash seed, and disable lazy inode-table and journal initialization. The APK
install log contains its execution time, so the build clears that transient log before
creating the filesystem. Two consecutive local builds produced the same hash. The
pinned container image does not pin APK repository contents: `apk add` resolves the
current package versions each run, so repository updates can still change the rootfs.

## Local build record

Measured on the development host on 2026-10-03 using rootless Podman 6.1.3:

- Rootfs image size: 134,217,728 bytes (128 MiB).
- First and second build time: 43.4 s and 36.5 s.
- SHA-256 for both builds: `3edc412fa85ed4da84fbfba3ba91f45cfc56a420f64f7d51351a4e433a5fc25a`.

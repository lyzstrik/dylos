# Guest images

Run `just images` (or `cargo xtask images`) from any directory in the workspace. It
verifies the local guest kernel before invoking rootless Podman; it never downloads the
kernel. Install the 6.18.51 kernel listed in `docs/host.md` at `kernels/vmlinux.bin`
first. `xtask/images/Dockerfile` builds the root filesystem in a pinned Alpine image
and exports only `rootfs.ext4` from a `scratch` final stage. The command passes the
reproducibility timestamp and image size as build args, exports into a temporary
directory under `target/images`, then moves the image to `rootfs.ext4` only after a
successful build; generated image files remain under ignored `target/`.

The root filesystem is installed from Alpine 3.23.6, pinned to the amd64 OCI image
`alpine@sha256:1f3591b8a02ea153f41c5bba878ad477f63ab3d19349762cb77504db02a23e15`.
It includes Alpine base utilities plus `iperf3`, `socat`, and `iproute2`. Its small
`/sbin/init` mounts proc, sysfs, and devtmpfs, then starts an interactive BusyBox shell
on serial console `ttyS0`.

The Dockerfile defaults `SOURCE_DATE_EPOCH` to `1700000000` and `ROOTFS_SIZE` to
`128M`. Reproducibility settings fix all staged file timestamps, the ext4 UUID and hash
seed, and disable lazy inode-table and journal initialization. The APK install log
contains its execution time, so the build clears that transient log before creating
the filesystem. The pinned container image does not pin APK repository contents:
`apk add` resolves the current package versions each run, so repository updates can
still change the rootfs. Pinning APK package versions or using a vendored APK cache is a
follow-up.

## Local build record

Measured on the development host on 2026-10-04 using rootless Podman 6.1.3:

- Rootfs image size: 134,217,728 bytes (128 MiB).
- First and second build time: 56.4 s and 3.6 s.
- First and second SHA-256: `b7260811f0d7dbd8971f2cca8c4fc664a215ef9b03d3519559cb19e4b5156089`;
  both runs matched.

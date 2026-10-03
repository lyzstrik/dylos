#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use std::env;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

const ALPINE_IMAGE: &str = "docker.io/library/alpine@sha256:1f3591b8a02ea153f41c5bba878ad477f63ab3d19349762cb77504db02a23e15";
const KERNEL_PATH: &str = "kernels/vmlinux.bin";
const KERNEL_SHA256: &str = "0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447";
const ROOTFS_SIZE: &str = "128M";
const SOURCE_DATE_EPOCH: &str = "1700000000";

fn main() {
    if let Err(error) = run() {
        eprintln!("xtask: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("e2e") => {
            eprintln!("xtask: e2e is not implemented yet");
            Ok(())
        }
        Some("images") => {
            if args.next().is_some() {
                bail!("images does not accept arguments");
            }
            build_images()
        }
        Some(subcommand) => {
            bail!("unknown subcommand '{subcommand}'");
        }
        None => {
            bail!("missing subcommand");
        }
    }
}

fn build_images() -> anyhow::Result<()> {
    verify_kernel(Path::new(KERNEL_PATH))?;

    let workspace = env::current_dir().context("cannot determine the workspace directory")?;
    let output_dir = workspace.join("target/images");
    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!(
            "cannot create image output directory {}",
            output_dir.display()
        )
    })?;
    let output_dir = output_dir.canonicalize().with_context(|| {
        format!(
            "cannot resolve image output directory {}",
            output_dir.display()
        )
    })?;

    let started = Instant::now();
    let status = Command::new("podman")
        .args(["run", "--rm", "--pull=missing", "--platform", "linux/amd64"])
        .arg("--volume")
        .arg(format!("{}:/out", output_dir.display()))
        .arg("--env")
        .arg(format!("SOURCE_DATE_EPOCH={SOURCE_DATE_EPOCH}"))
        .arg("--env")
        .arg(format!("ROOTFS_SIZE={ROOTFS_SIZE}"))
        .args([ALPINE_IMAGE, "sh", "-euxc"])
        .arg(BUILD_SCRIPT)
        .status()
        .context("could not run podman; install rootless podman to build guest images")?;
    if !status.success() {
        bail!("podman failed to build the Alpine rootfs (exit status {status})");
    }

    let output = output_dir.join("rootfs.ext4");
    let size = std::fs::metadata(&output)
        .with_context(|| format!("podman did not create {}", output.display()))?
        .len();
    let digest = sha256(&output)?;
    eprintln!(
        "built {} ({size} bytes, SHA-256 {digest}) in {:.1}s",
        output.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn verify_kernel(path: &Path) -> anyhow::Result<()> {
    if !path.is_file() {
        bail!(
            "guest kernel {} is missing; provide the 6.18.51 kernel from docs/host.md (SHA-256 {KERNEL_SHA256}); images does not download kernels",
            path.display()
        );
    }
    let actual = sha256(path)?;
    if actual != KERNEL_SHA256 {
        bail!(
            "guest kernel {} has SHA-256 {actual}, expected {KERNEL_SHA256} (6.18.51 from docs/host.md)",
            path.display()
        );
    }
    Ok(())
}

fn sha256(path: &Path) -> anyhow::Result<String> {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .with_context(|| format!("could not calculate SHA-256 for {}", path.display()))?;
    if !output.status.success() {
        bail!("sha256sum failed for {}", path.display());
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("sha256sum returned invalid output for {}", path.display()))?;
    let digest = stdout
        .split_whitespace()
        .next()
        .with_context(|| format!("sha256sum returned no digest for {}", path.display()))?;
    Ok(digest.to_owned())
}

const BUILD_SCRIPT: &str = r#"
apk add --no-cache e2fsprogs
mkdir -p /rootfs
apk --root /rootfs --initdb --keys-dir /etc/apk/keys \
    --repositories-file /etc/apk/repositories add --no-cache \
    alpine-base iperf3 socat iproute2
mkdir -p /rootfs/proc /rootfs/sys /rootfs/dev /rootfs/sbin
rm -f /rootfs/sbin/init
: > /rootfs/var/log/apk.log
cat > /rootfs/sbin/init <<'INIT'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
exec setsid cttyhack /bin/sh -i </dev/ttyS0 >/dev/ttyS0 2>&1
INIT
chmod 0755 /rootfs/sbin/init
find /rootfs -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
E2FSPROGS_FAKE_TIME="$SOURCE_DATE_EPOCH" mkfs.ext4 -F -q -d /rootfs \
    -U 01234567-89ab-cdef-0123-456789abcdef \
    -E hash_seed=01234567-89ab-cdef-0123-456789abcdef,lazy_itable_init=0,lazy_journal_init=0 \
    /out/rootfs.ext4 "$ROOTFS_SIZE"
"#;

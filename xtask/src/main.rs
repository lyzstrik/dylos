#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use std::env;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

const KERNEL_PATH: &str = "kernels/vmlinux.bin";
const KERNEL_SHA256: &str = "0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447";
const ROOTFS_SIZE: &str = "128M";
const ROOTFS_BUILD_TEMP: &str = "rootfs-build.tmp";
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
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("cannot determine the workspace directory from CARGO_MANIFEST_DIR")?;
    verify_kernel(&workspace.join(KERNEL_PATH))?;

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

    let temporary = output_dir.join(ROOTFS_BUILD_TEMP);
    let output = output_dir.join("rootfs.ext4");
    remove_temporary(&temporary)?;
    std::fs::create_dir(&temporary).with_context(|| {
        format!(
            "cannot create temporary build directory {}",
            temporary.display()
        )
    })?;

    let started = Instant::now();
    let build_result = build_rootfs(workspace, &temporary, &output);
    let cleanup_result = remove_temporary(&temporary);
    let (size, digest) = match (build_result, cleanup_result) {
        (Ok(result), Ok(())) => result,
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cleanup_error)) => {
            return Err(error.context(format!(
                "also failed to remove temporary build directory: {cleanup_error:#}"
            )));
        }
        (Ok(_), Err(cleanup_error)) => return Err(cleanup_error),
    };
    eprintln!(
        "built {} ({size} bytes, SHA-256 {digest}) in {:.1}s",
        output.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn build_rootfs(
    workspace: &Path,
    temporary: &Path,
    output: &Path,
) -> anyhow::Result<(u64, String)> {
    let images_dir = workspace.join("xtask/images");
    let output_spec = format!("type=local,dest={}", temporary.display());
    let status = Command::new("podman")
        .args(["build", "--platform", "linux/amd64"])
        .arg("--build-arg")
        .arg(format!("SOURCE_DATE_EPOCH={SOURCE_DATE_EPOCH}"))
        .arg("--build-arg")
        .arg(format!("ROOTFS_SIZE={ROOTFS_SIZE}"))
        .arg("--output")
        .arg(output_spec)
        .arg("--file")
        .arg(images_dir.join("Dockerfile"))
        .arg(&images_dir)
        .status()
        .context("could not run podman; install rootless podman to build guest images")?;
    if !status.success() {
        bail!("podman failed to build the guest rootfs (exit status {status})");
    }

    let image = temporary.join("rootfs.ext4");
    let size = std::fs::metadata(&image)
        .with_context(|| format!("podman did not create {}", image.display()))?
        .len();
    let digest = sha256(&image)?;
    std::fs::rename(&image, output).with_context(|| {
        format!(
            "cannot move built image {} to {}",
            image.display(),
            output.display()
        )
    })?;
    Ok((size, digest))
}

fn remove_temporary(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("cannot remove temporary build directory {}", path.display())),
    }
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

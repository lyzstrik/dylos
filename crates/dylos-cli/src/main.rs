//! The `dylos` CLI binary.
//!
//! Provides the operator entry point to launch and manage multi-machine
//! Firecracker microVM labs.
#![forbid(unsafe_code)]

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use dylos_core::LabSpec;
use dylos_runtime::{Fleet, Lab, LabConfig, LabError};
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::{Mode, SFlag, fstat};

const DEFAULT_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

fn teardown_timeout() -> Duration {
    std::env::var("DYLOS_TEARDOWN_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(DEFAULT_TEARDOWN_TIMEOUT, Duration::from_millis)
}

fn validate_lab_id(id: &str) -> Result<()> {
    dylos_core::validate_name(id, 32).map_err(|reason| anyhow!("invalid lab id '{id}': {reason}"))
}

fn pin_control_socket(base: &Path, id: &str) -> std::io::Result<File> {
    let mut dir = File::open(if base.is_absolute() { "/" } else { "." })?;
    for component in base.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                dir = File::from(openat(
                    &dir,
                    name,
                    OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )?);
            }
            _ => return Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
    let lab_dir = File::from(openat(
        &dir,
        id,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    let socket = File::from(openat(
        &lab_dir,
        "control.sock",
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    let stat = fstat(&socket)?;
    if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFSOCK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "control socket is not a socket (symlinks and regular files are refused)",
        ));
    }
    Ok(socket)
}

#[derive(Parser, Debug)]
#[command(
    name = "dylos",
    version,
    about = "Engine for reproducible microVM lab networks",
    after_help = "Environment variables:\n  DYLOS_FC_BIN_DIR  Directory containing firecracker and jailer binaries\n  DYLOS_LAB_DIR     Base directory for lab state storage (overrides target/labs)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage lab environments
    Lab(LabArgs),
}

#[derive(Args, Debug)]
struct LabArgs {
    #[command(subcommand)]
    command: LabCommands,
}

#[derive(Subcommand, Debug)]
enum LabCommands {
    /// Bring up a lab and supervise it until teardown
    Up(UpArgs),
    /// Restore a snapshot directory into a new supervised lab
    Restore(RestoreArgs),
    /// Fork a snapshot into N parallel independent clone labs
    Fork(ForkArgs),
    /// Tear down a running or crashed lab
    Down(DownArgs),
    /// Snapshot a running lab's microVMs and disks
    Snapshot(SnapshotArgs),
}

#[derive(Args, Debug)]
struct ForkArgs {
    /// Snapshot directory containing manifest.json
    snapshot: PathBuf,

    /// Number of clones to fork
    #[arg(short = 'n', long = "count")]
    count: usize,

    /// Base prefix for clone identifiers (defaults to snapshot directory name or lab name)
    #[arg(long, visible_alias = "id")]
    prefix: Option<String>,
}

#[derive(Args, Debug)]
struct RestoreArgs {
    /// Snapshot directory containing manifest.json
    snapshot: PathBuf,
    /// New lab identifier (defaults to `restore-<snapshot directory name>`)
    #[arg(long)]
    id: Option<String>,
}

#[derive(Args, Debug)]
struct SnapshotArgs {
    /// Lab identifier to snapshot
    id: String,

    /// Optional snapshot identifier (defaults to `snap-<timestamp>`)
    #[arg(long)]
    snapshot_id: Option<String>,
}

#[derive(Args, Debug)]
struct UpArgs {
    /// Path to the lab specification YAML file
    spec: PathBuf,

    /// Lab identifier (defaults to spec file stem)
    #[arg(long)]
    id: Option<String>,
}

#[derive(Args, Debug)]
struct DownArgs {
    /// Lab identifier to tear down
    id: String,
}

fn build_config() -> LabConfig {
    let mut config = LabConfig::default();
    if let Some(bin_dir) = std::env::var_os("DYLOS_FC_BIN_DIR").filter(|d| !d.is_empty()) {
        let bin = PathBuf::from(bin_dir);
        config.jailer.jailer = bin.join("jailer");
        config.jailer.firecracker = bin.join("firecracker");
    }
    if let Some(lab_dir) = std::env::var_os("DYLOS_LAB_DIR").filter(|d| !d.is_empty()) {
        config.store.base = PathBuf::from(lab_dir);
    }
    config
}

fn init_tracing() {
    let filter = match tracing_subscriber::EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => tracing_subscriber::EnvFilter::new("info"),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Commands::Lab(lab_args) => match lab_args.command {
            LabCommands::Up(up_args) => lab_up(up_args).await,
            LabCommands::Restore(args) => lab_restore(args).await,
            LabCommands::Fork(args) => lab_fork(args).await,
            LabCommands::Down(down_args) => lab_down(down_args).await,
            LabCommands::Snapshot(snapshot_args) => lab_snapshot(snapshot_args).await,
        },
    }
}

async fn lab_up(args: UpArgs) -> Result<()> {
    let id = match args.id {
        Some(id) => id,
        None => args
            .spec
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "could not determine lab id from spec file stem: {}",
                    args.spec.display()
                )
            })?
            .to_string(),
    };
    validate_lab_id(&id)?;

    // Register signal handlers before bring-up so signals during startup
    // are caught instead of triggering default unhandled process termination.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("failed to register SIGINT handler")?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;
    tracing::info!(lab = %id, "registered termination signal handlers for bring-up");

    let config = build_config();
    let up_future = async {
        let spec_content = tokio::fs::read_to_string(&args.spec)
            .await
            .with_context(|| format!("failed to read lab spec from {}", args.spec.display()))?;
        let spec = LabSpec::from_yaml_str(&spec_content)
            .with_context(|| format!("failed to parse lab spec from {}", args.spec.display()))?;
        Lab::up_with(&config, &spec, &id)
            .await
            .with_context(|| format!("failed to bring up lab '{id}'"))
    };
    tokio::pin!(up_future);

    let mut interrupted = false;
    let lab = loop {
        tokio::select! {
            // Consume ready signals before a simultaneous bring-up error can exit.
            biased;
            _ = sigint.recv() => {
                tracing::info!(lab = %id, "received Ctrl+C (SIGINT) during bring-up, will tear down once ready");
                interrupted = true;
            }
            _ = sigterm.recv() => {
                tracing::info!(lab = %id, "received SIGTERM during bring-up, will tear down once ready");
                interrupted = true;
            }
            res = &mut up_future => {
                break res?;
            }
        }
    };

    if !interrupted {
        println!("lab {id} is up, press Ctrl+C or run `dylos lab down {id}`");
    }
    supervise(&config, vec![lab], interrupted, &mut sigint, &mut sigterm).await
}

async fn lab_restore(args: RestoreArgs) -> Result<()> {
    let id = args.id.unwrap_or_else(|| {
        format!(
            "restore-{}",
            args.snapshot
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("snapshot")
        )
    });
    validate_lab_id(&id)?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("failed to register SIGINT handler")?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;
    let config = build_config();
    let restore = Lab::restore_with(&config, &args.snapshot, &id);
    tokio::pin!(restore);
    let mut interrupted = false;
    let lab = loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => interrupted = true,
            _ = sigterm.recv() => interrupted = true,
            result = &mut restore => break result.with_context(|| format!("failed to restore lab '{id}'"))?,
        }
    };
    if !interrupted {
        println!("lab {id} restored, press Ctrl+C or run `dylos lab down {id}`");
    }
    supervise(&config, vec![lab], interrupted, &mut sigint, &mut sigterm).await
}

pub(crate) fn derive_clone_prefix(snapshot: &Path) -> String {
    // Normal Store layout: <base>/snapshots/<lab_id>/<snap_id>
    if let Some(lab_name) = snapshot
        .parent()
        .filter(|p| {
            p.parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "snapshots")
        })
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
    {
        return lab_name.to_string();
    }
    // Alternative layout: <base>/<lab_id>/snapshots/<snap_id>
    if let Some(lab_name) = snapshot
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "snapshots"))
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
    {
        return lab_name.to_string();
    }
    snapshot
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("clone")
        .to_string()
}

async fn lab_fork(args: ForkArgs) -> Result<()> {
    if args.count == 0 {
        bail!("clone count must be at least 1");
    }
    let prefix = args
        .prefix
        .unwrap_or_else(|| derive_clone_prefix(&args.snapshot));
    let clone_ids = Lab::generate_clone_ids(&prefix, args.count).map_err(|e| anyhow!("{e}"))?;

    for id in &clone_ids {
        validate_lab_id(id)?;
    }

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("failed to register SIGINT handler")?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;

    let config = build_config();
    let fork_future = Lab::fork_with(&config, &args.snapshot, &clone_ids);
    tokio::pin!(fork_future);

    let mut interrupted = false;
    let labs = loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => interrupted = true,
            _ = sigterm.recv() => interrupted = true,
            res = &mut fork_future => {
                break res.with_context(|| format!("failed to fork snapshot '{}'", args.snapshot.display()))?;
            }
        }
    };

    if !interrupted {
        for lab in &labs {
            let latency_ms = lab.restore_latency().map_or(0, |d| d.as_millis());
            let pss = match lab.total_pss_kib().await {
                Ok(kib) => format!("{kib} KiB"),
                Err(e) => {
                    tracing::warn!(lab = %lab.id(), error = %e, "could not measure clone PSS");
                    "unavailable".to_string()
                }
            };
            println!(
                "clone {}: restored in {latency_ms} ms, PSS: {pss}",
                lab.id()
            );
        }
        println!(
            "all {} clones ready, press Ctrl+C or run `dylos lab down <clone>`",
            labs.len()
        );
    }
    supervise(&config, labs, interrupted, &mut sigint, &mut sigterm).await
}

/// Supervises `labs` until each is torn down, or tears all of them down on a signal, or at once
/// if one arrived during bring-up.
async fn supervise(
    config: &LabConfig,
    labs: Vec<Lab>,
    interrupted: bool,
    sigint: &mut tokio::signal::unix::Signal,
    sigterm: &mut tokio::signal::unix::Signal,
) -> Result<()> {
    let mut fleet = Fleet::new(config, teardown_timeout());
    for lab in labs {
        fleet.spawn(lab);
    }
    let shutdown = async {
        if interrupted {
            return;
        }
        tokio::select! {
            _ = sigint.recv() => tracing::info!("received Ctrl+C (SIGINT), requesting teardown"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM, requesting teardown"),
        }
    };
    Ok(fleet.run(shutdown).await?)
}

async fn lab_down(args: DownArgs) -> Result<()> {
    validate_lab_id(&args.id)?;

    let config = build_config();
    let timeout = teardown_timeout();

    let pinned = pin_control_socket(&config.store.base, &args.id);
    let teardown_res = match pinned {
        Ok(socket_file) => {
            let socket_path = PathBuf::from(format!("/proc/self/fd/{}", socket_file.as_raw_fd()));
            let res = Lab::request_teardown(&socket_path, timeout).await;
            drop(socket_file);
            res
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(LabError::Io {
            lab: args.id.clone(),
            source: err,
        }),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to access control socket for lab '{}' (symlinks and unsafe paths are refused)",
                    args.id
                )
            });
        }
    };

    match teardown_res {
        Ok(()) => {
            println!("lab {} is down", args.id);
            Ok(())
        }
        Err(LabError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound
                || source.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            tracing::info!(
                lab = %args.id,
                "supervisor not responding on control socket ({source}), falling back to teardown by name"
            );
            Lab::teardown_by_name(&config, &args.id)
                .await
                .with_context(|| format!("failed to tear down lab '{}' by name", args.id))?;
            println!("lab {} is down", args.id);
            Ok(())
        }
        Err(err) => {
            Err(err).with_context(|| format!("failed to request teardown for lab '{}'", args.id))
        }
    }
}

async fn lab_snapshot(args: SnapshotArgs) -> Result<()> {
    validate_lab_id(&args.id)?;
    if let Some(ref snap_id) = args.snapshot_id {
        dylos_core::validate_name(snap_id, 32)
            .map_err(|reason| anyhow!("invalid snapshot id '{snap_id}': {reason}"))?;
    }

    let config = build_config();
    // Total deadline gives 60s budget beyond the 120s Firecracker API snapshot deadline
    let timeout = Duration::from_secs(180);

    let pinned = pin_control_socket(&config.store.base, &args.id);
    let socket_file = match pinned {
        Ok(f) => f,
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to access control socket for lab '{}' (symlinks and unsafe paths are refused)",
                    args.id
                )
            });
        }
    };

    let socket_path = PathBuf::from(format!("/proc/self/fd/{}", socket_file.as_raw_fd()));
    let res = Lab::request_snapshot(&socket_path, args.snapshot_id.as_deref(), timeout).await;
    drop(socket_file);

    let snap_id = match res {
        Ok(id) => id,
        Err(err) => {
            if err.to_string().contains("timed out") || err.to_string().contains("TimedOut") {
                bail!(
                    "snapshot request timed out after {timeout:?}; supervisor outcome is unknown (do not assume rollback): {err}"
                );
            }
            return Err(err).with_context(|| format!("failed to snapshot lab '{}'", args.id));
        }
    };
    println!("lab {} snapshotted successfully as {}", args.id, snap_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_clone_prefix() {
        // Normal Store layout: <base>/snapshots/<lab_id>/<snap_id>
        assert_eq!(
            derive_clone_prefix(Path::new("/var/lib/dylos/snapshots/my-lab/snap-1")),
            "my-lab"
        );
        // Alternative layout: <base>/<lab_id>/snapshots/<snap_id>
        assert_eq!(
            derive_clone_prefix(Path::new("/var/lib/dylos/my-lab/snapshots/snap-1")),
            "my-lab"
        );
        // Flat snapshot folder
        assert_eq!(derive_clone_prefix(Path::new("my-snapshot")), "my-snapshot");
        // Empty path fallback
        assert_eq!(derive_clone_prefix(Path::new("")), "clone");
    }
}

//! The `dylos` CLI binary.
//!
//! Provides the operator entry point to launch and manage multi-machine
//! Firecracker microVM labs.
#![forbid(unsafe_code)]

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use dylos_core::LabSpec;
use dylos_runtime::{Lab, LabConfig, LabError};
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
    /// Tear down a running or crashed lab
    Down(DownArgs),
    /// Snapshot a running lab's microVMs and disks
    Snapshot(SnapshotArgs),
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

    let socket = lab.control_socket();
    let supervisor = tokio::spawn(lab.supervise());

    if interrupted {
        tracing::info!(lab = %id, "termination was requested during bring-up, requesting teardown");
        return handle_signal_teardown(&id, &socket, supervisor).await;
    }

    println!("lab {id} is up, press Ctrl+C or run `dylos lab down {id}`");

    let mut supervisor = supervisor;
    tokio::select! {
        res = &mut supervisor => {
            res.context("supervisor task panicked")?
                .with_context(|| format!("supervision of lab '{id}' failed"))?;
            Ok(())
        }
        _ = sigint.recv() => {
            tracing::info!(lab = %id, "received Ctrl+C (SIGINT), requesting teardown");
            handle_signal_teardown(&id, &socket, supervisor).await
        }
        _ = sigterm.recv() => {
            tracing::info!(lab = %id, "received SIGTERM, requesting teardown");
            handle_signal_teardown(&id, &socket, supervisor).await
        }
    }
}

async fn handle_signal_teardown(
    id: &str,
    socket: &Path,
    supervisor: tokio::task::JoinHandle<std::result::Result<(), LabError>>,
) -> Result<()> {
    let timeout = teardown_timeout();
    let teardown_res = Lab::request_teardown(socket, timeout).await;
    let supervise_res = supervisor.await;
    teardown_res.with_context(|| format!("failed to request teardown for lab '{id}'"))?;
    supervise_res
        .context("supervisor task panicked")?
        .with_context(|| format!("supervision of lab '{id}' failed"))?;
    Ok(())
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
        dylos_core::validate_name(snap_id, 64)
            .map_err(|reason| anyhow!("invalid snapshot id '{snap_id}': {reason}"))?;
    }

    let config = build_config();
    let timeout = Duration::from_secs(120);

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

    let snap_id = res.with_context(|| format!("failed to snapshot lab '{}'", args.id))?;
    println!("lab {} snapshotted successfully as {}", args.id, snap_id);
    Ok(())
}

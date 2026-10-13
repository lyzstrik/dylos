//! The `dylos` CLI binary.
//!
//! Provides the operator entry point to launch and manage multi-machine
//! Firecracker microVM labs.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use dylos_core::LabSpec;
use dylos_runtime::{Lab, LabConfig, LabError};

const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

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

fn control_socket_path(config: &LabConfig, id: &str) -> PathBuf {
    config.store.base.join(id).join("control.sock")
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

    let spec_content = tokio::fs::read_to_string(&args.spec)
        .await
        .with_context(|| format!("failed to read lab spec from {}", args.spec.display()))?;
    let spec = LabSpec::from_yaml_str(&spec_content)
        .with_context(|| format!("failed to parse lab spec from {}", args.spec.display()))?;

    let config = build_config();
    let lab = Lab::up_with(&config, &spec, &id)
        .await
        .with_context(|| format!("failed to bring up lab '{id}'"))?;

    let socket = lab.control_socket();
    println!("lab {id} is up, press Ctrl+C or run `dylos lab down {id}`");

    let mut supervisor = tokio::spawn(lab.supervise());

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;

    let supervise_res = tokio::select! {
        res = &mut supervisor => {
            res
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!(lab = %id, "received Ctrl+C (SIGINT), requesting teardown");
            let teardown_res = Lab::request_teardown(&socket, TEARDOWN_TIMEOUT).await;
            let supervise_res = supervisor.await;
            teardown_res.with_context(|| format!("failed to request teardown for lab '{id}'"))?;
            supervise_res
        }
        _ = sigterm.recv() => {
            tracing::info!(lab = %id, "received SIGTERM, requesting teardown");
            let teardown_res = Lab::request_teardown(&socket, TEARDOWN_TIMEOUT).await;
            let supervise_res = supervisor.await;
            teardown_res.with_context(|| format!("failed to request teardown for lab '{id}'"))?;
            supervise_res
        }
    };

    supervise_res
        .context("supervisor task panicked")?
        .with_context(|| format!("supervision of lab '{id}' failed"))?;

    Ok(())
}

async fn lab_down(args: DownArgs) -> Result<()> {
    let config = build_config();
    let socket = control_socket_path(&config, &args.id);

    match Lab::request_teardown(&socket, TEARDOWN_TIMEOUT).await {
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

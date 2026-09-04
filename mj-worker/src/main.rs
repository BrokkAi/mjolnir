//! Portable target-side entry point for Mjolnir session workers.

#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use hel::hel_worker_launch::WorkerLaunchConfig;
use mj_worker::hel_worker_runtime::{
    AcpSupervisorSpec, lead_process_group, prepare_managed_harness, proxy, run_acp_supervisor,
    run_daemon,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "mj-worker", version, about = "Mjolnir target-side worker")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Internal target-side worker commands.
    #[command(hide = true)]
    Worker(WorkerArgs),
}

#[derive(Debug, Args)]
struct WorkerArgs {
    #[command(subcommand)]
    command: WorkerCommand,
}

#[derive(Debug, Subcommand)]
enum WorkerCommand {
    /// Own an ACP bridge and durable session event log.
    Run {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    /// Prepare an exact managed harness without starting a session worker.
    PrepareHarness {
        #[arg(long)]
        config: PathBuf,
    },
    /// Proxy JSON-lines between stdio and a detached worker.
    Proxy {
        #[arg(long)]
        root: PathBuf,
    },
    /// Supervise the ACP bridge process tree for a worker daemon.
    AcpSupervisor {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Build a target-side archive for verified controller transfer.
    ExportCheckpoint {
        /// Export specification path, or `-` to read it from standard input.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Seal target-owned checkpoint inputs while ACP dispatch is frozen.
    #[command(hide = true)]
    CaptureCheckpoint,
    /// Package a sealed checkpoint after ACP dispatch has resumed.
    #[command(hide = true)]
    PackCheckpoint,
    /// Restore a verified archive into a freshly cloned target.
    RestoreCheckpoint {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Restore controller-side local repository bootstrap snapshots.
    RestoreRepositories {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Install one streamed resource directory on a remote target.
    InstallResource {
        #[arg(long)]
        destination: PathBuf,
    },
    /// Serve project memory tools over MCP stdio.
    MemoryMcp {
        #[arg(long)]
        root: PathBuf,
    },
    /// Serve the turn review's specialist-dispatch tool over MCP stdio.
    ReviewMcp {
        #[arg(long)]
        socket: PathBuf,
    },
    /// Bridge controller Git services to this worker over stdio.
    GitBridge {
        #[arg(long)]
        root: PathBuf,
    },
    /// Expose one bridged repository as a Git ext transport.
    GitProxy {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        repository: String,
        service: String,
    },
}

fn write_worker_exit_record(root: &Path, reason: &str) {
    if !root.is_dir() {
        return;
    }
    let record = serde_json::json!({
        "reason": reason,
        "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "version": env!("CARGO_PKG_VERSION"),
    });
    let bytes = match serde_json::to_vec_pretty(&record) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("Mjolnir: could not serialize worker exit record: {error}");
            return;
        }
    };
    if let Err(error) = std::fs::write(root.join("worker-exit.json"), bytes) {
        eprintln!("Mjolnir: could not write worker exit record: {error}");
    }
}

fn install_worker_last_words(root: &Path) {
    let root = root.to_path_buf();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_worker_exit_record(&root, &format!("panic: {info}"));
        default_hook(info);
    }));
}

fn install_stderr_logging() -> Result<()> {
    let (filter, filter_error) = match std::env::var("RUST_LOG") {
        Ok(value) => match EnvFilter::try_new(value) {
            Ok(filter) => (filter, None),
            Err(error) => (EnvFilter::new("warn"), Some(error.to_string())),
        },
        Err(std::env::VarError::NotPresent) => (EnvFilter::new("warn"), None),
        Err(error @ std::env::VarError::NotUnicode(_)) => {
            (EnvFilter::new("warn"), Some(error.to_string()))
        }
    };
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("install Mjolnir stderr subscriber: {error}"))?;
    if let Some(error) = filter_error {
        tracing::warn!(%error, "ignored invalid RUST_LOG filter");
    }
    Ok(())
}

fn main() -> Result<()> {
    install_stderr_logging()?;
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime")?;
    let result = runtime.block_on(run_command(cli.command));
    if let Err(error) = &result {
        tracing::error!(
            error = format!("{error:#}"),
            "Mjolnir worker exited with an error"
        );
    }
    result
}

async fn run_command(command: Command) -> Result<()> {
    let Command::Worker(args) = command;
    match args.command {
        WorkerCommand::Run { root, config } => {
            lead_process_group();
            install_worker_last_words(&root);
            let result = run_daemon(root.clone(), WorkerLaunchConfig::read(&config)?).await;
            if let Err(error) = &result {
                write_worker_exit_record(&root, &format!("{error:#}"));
            }
            result
        }
        WorkerCommand::PrepareHarness { config } => {
            prepare_managed_harness(WorkerLaunchConfig::read(&config)?).await
        }
        WorkerCommand::Proxy { root } => proxy(root).await,
        WorkerCommand::AcpSupervisor { spec } => {
            run_acp_supervisor(AcpSupervisorSpec::read(&spec)?).await
        }
        WorkerCommand::ExportCheckpoint { spec } => {
            let checkpoint = hel::hel_checkpoint::export_from_spec_file(&spec)?;
            println!("{}", serde_json::to_string(&checkpoint)?);
            Ok(())
        }
        WorkerCommand::CaptureCheckpoint => {
            let checkpoint =
                hel::hel_checkpoint::capture_from_spec_reader(&mut std::io::stdin().lock())?;
            println!("{}", serde_json::to_string(&checkpoint)?);
            Ok(())
        }
        WorkerCommand::PackCheckpoint => {
            let checkpoint =
                hel::hel_checkpoint::pack_from_spec_reader(&mut std::io::stdin().lock())?;
            println!("{}", serde_json::to_string(&checkpoint)?);
            Ok(())
        }
        WorkerCommand::RestoreCheckpoint { spec } => {
            hel::hel_checkpoint::restore_from_spec_file(&spec)
        }
        WorkerCommand::RestoreRepositories { spec } => {
            hel::hel_checkpoint::restore_repositories_from_spec_file(&spec)
        }
        WorkerCommand::InstallResource { destination } => {
            hel::hel_resources::install_resource_stream(std::io::stdin(), &destination)
        }
        WorkerCommand::MemoryMcp { root } => hel::hel_project_memory::run_mcp_stdio(&root),
        WorkerCommand::ReviewMcp { socket } => hel::hel_review::mcp::run_mcp_stdio(&socket),
        WorkerCommand::GitBridge { root } => hel::hel_git_proxy::run_worker_bridge(&root).await,
        WorkerCommand::GitProxy {
            root,
            repository,
            service,
        } => hel::hel_git_proxy::run_worker_proxy(&root, &repository, &service).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_shape_stays_compatible_with_installed_launches() {
        let cli = Cli::try_parse_from([
            "hel",
            "worker",
            "run",
            "--root",
            "/worker",
            "--config",
            "/worker/launch.json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Worker(WorkerArgs {
                command: WorkerCommand::Run { .. }
            })
        ));
    }

    #[test]
    fn managed_harness_preparation_command_is_target_side() {
        let cli = Cli::try_parse_from([
            "hel",
            "worker",
            "prepare-harness",
            "--config",
            "/worker/launch.json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Worker(WorkerArgs {
                command: WorkerCommand::PrepareHarness { .. }
            })
        ));
    }

    #[test]
    fn streaming_checkpoint_worker_commands_parse_without_file_arguments() {
        for name in ["capture-checkpoint", "pack-checkpoint"] {
            let cli = Cli::try_parse_from(["hel", "worker", name]).unwrap();
            assert!(matches!(
                cli.command,
                Command::Worker(WorkerArgs {
                    command: WorkerCommand::CaptureCheckpoint | WorkerCommand::PackCheckpoint
                })
            ));
        }
    }

    #[test]
    fn export_checkpoint_accepts_stdin_spec_marker() {
        let cli =
            Cli::try_parse_from(["hel", "worker", "export-checkpoint", "--spec", "-"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Worker(WorkerArgs {
                command: WorkerCommand::ExportCheckpoint { spec }
            }) if spec == Path::new("-")
        ));
    }
}

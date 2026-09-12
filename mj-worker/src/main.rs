//! Portable target-side entry point for Mjolnir session workers.

#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use mj_core::archive::{EXPORT_REFUSED_EXIT_CODE, PushBranchError, SessionExportError};
use mj_core::worker_launch::WorkerLaunchConfig;
use mj_worker::worker_runtime::{
    AcpSupervisorSpec, lead_process_group, prepare_managed_harness, proxy, run_acp_supervisor,
    run_daemon,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "mj-worker", version, about = "Mjolnir target-side worker")]
struct Cli {
    /// Internal handoff: the parent explicitly supplied the clean login snapshot.
    #[arg(long, hide = true, global = true)]
    login_environment_ready: bool,
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
    /// Discover profile model choices without submitting a prompt.
    DiscoverConfig {
        #[arg(long)]
        spec: PathBuf,
    },
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
    /// Print a unified diff of the session's work in one repository.
    Diff {
        #[arg(long)]
        repository: PathBuf,
        /// The commit the session started from, when the controller recorded one.
        #[arg(long)]
        base: Option<String>,
        /// The session branch, whose reflog names the commit it was created at.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Write one file from the session workspace to standard output.
    ReadFile {
        #[arg(long)]
        root: PathBuf,
        /// Path relative to the workspace root.
        #[arg(long)]
        path: PathBuf,
    },
    /// Atomically publish stdin as a file in the session workspace.
    WriteFile {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        overwrite: bool,
        /// Expected input length; premature EOF must not publish a partial file.
        #[arg(long)]
        length: usize,
    },
    /// Push the repository's current HEAD to a branch on its push remote.
    PushBranch {
        /// Worker root containing the session's existing Git authentication.
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        branch: String,
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

/// Clean the worker itself as well as its harnesses: Git and other target-side
/// helpers must not inherit controller or build-tool variables either.
fn bootstrap_login_environment(cli: &Cli) -> Result<()> {
    if cli.login_environment_ready {
        return mj_core::login_environment::initialize_from_parent();
    }
    let Command::Worker(args) = &cli.command;
    let needs_login = match &args.command {
        WorkerCommand::Run { config, .. } => Some(
            mj_core::worker_launch::WorkerLaunchConfig::read(config)?.run_mode
                != mj_core::worker_launch::WorkerRunMode::CheckpointOnly,
        ),
        WorkerCommand::PrepareHarness { .. }
        | WorkerCommand::DiscoverConfig { .. }
        | WorkerCommand::Diff { .. }
        | WorkerCommand::PushBranch { .. } => Some(true),
        _ => None,
    };
    let Some(needs_login) = needs_login else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut environment = if needs_login {
            mj_core::login_environment::discover()?
        } else {
            mj_core::login_environment::bootstrap()?
        };
        if let WorkerCommand::Run { config, .. } | WorkerCommand::PrepareHarness { config } =
            &args.command
        {
            environment.extend(WorkerLaunchConfig::read(config)?.target_environment);
        }
        if let WorkerCommand::PushBranch { root, .. } = &args.command {
            environment.extend(
                WorkerLaunchConfig::read(&root.join("launch.json"))
                    .context("load branch export target settings; resume the session to restore its setup")?
                    .target_environment,
            );
            mj_worker::worker_runtime::attach_session_git_environment(root, &mut environment)?;
        }
        let executable = if cfg!(target_os = "linux") {
            PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe()?
        };
        let mut arguments = std::env::args_os();
        let argv0 = arguments
            .next()
            .context("worker executable argument is missing")?;
        let error = std::process::Command::new(executable)
            // Lifecycle probes identify the installed `hel worker run --root`
            // prefix. Keep it intact across re-exec, including argv[0].
            .arg0(argv0)
            .args(arguments)
            .arg("--login-environment-ready")
            .env_clear()
            .envs(environment)
            .exec();
        Err(error).context("start worker with target login environment")
    }
    #[cfg(not(unix))]
    anyhow::bail!("target login environment requires a Unix worker")
}

fn main() -> Result<()> {
    install_stderr_logging()?;
    let cli = Cli::parse();
    if let Err(error) = bootstrap_login_environment(&cli) {
        let Command::Worker(args) = &cli.command;
        if let WorkerCommand::Run { root, .. } = &args.command {
            write_worker_exit_record(root, &format!("initialize worker environment: {error:#}"));
        }
        return Err(error);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime")?;
    let result = runtime.block_on(run_command(cli.command));
    if let Err(error) = &result
        && let Some(refusal) = error.downcast_ref::<ExportRefused>()
    {
        // A refusal is a precondition the caller can fix, so it leaves the
        // process with its own exit code and the reason on standard error;
        // the daemon turns that pair into a 409 rather than a 500.
        eprintln!("{refusal}");
        std::process::exit(EXPORT_REFUSED_EXIT_CODE);
    }
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
        WorkerCommand::DiscoverConfig { spec } => {
            let spec = serde_json::from_slice(&std::fs::read(spec)?)?;
            let config = mj_worker::worker_runtime::discover_profile_config(spec).await?;
            println!("{}", serde_json::to_string(&config)?);
            Ok(())
        }
        WorkerCommand::AcpSupervisor { spec } => {
            run_acp_supervisor(AcpSupervisorSpec::read(&spec)?).await
        }
        WorkerCommand::ExportCheckpoint { spec } => {
            let checkpoint = mj_worker::checkpoint::export_from_spec_file(&spec)?;
            println!("{}", serde_json::to_string(&checkpoint)?);
            Ok(())
        }
        WorkerCommand::CaptureCheckpoint => {
            let checkpoint =
                mj_worker::checkpoint::capture_from_spec_reader(&mut std::io::stdin().lock())?;
            println!("{}", serde_json::to_string(&checkpoint)?);
            Ok(())
        }
        WorkerCommand::PackCheckpoint => {
            let checkpoint =
                mj_worker::checkpoint::pack_from_spec_reader(&mut std::io::stdin().lock())?;
            println!("{}", serde_json::to_string(&checkpoint)?);
            Ok(())
        }
        WorkerCommand::RestoreCheckpoint { spec } => {
            mj_worker::checkpoint::restore_from_spec_file(&spec)
        }
        WorkerCommand::InstallResource { destination } => {
            mj_core::resources::install_resource_stream(std::io::stdin(), &destination)
        }
        WorkerCommand::MemoryMcp { root } => mj_worker::memory_mcp::run_mcp_stdio(&root),
        WorkerCommand::ReviewMcp { socket } => mj_worker::review::mcp::run_mcp_stdio(&socket),
        WorkerCommand::Diff {
            repository,
            base,
            branch,
        } => {
            let diff = mj_core::archive::session_diff(
                &mj_core::archive::SystemGit,
                &repository,
                base.as_deref(),
                branch.as_deref(),
            )
            .map_err(export_error)?;
            write_stdout(diff.as_bytes())
        }
        WorkerCommand::ReadFile { root, path } => {
            write_stdout(&mj_core::archive::read_session_file(&root, &path).map_err(export_error)?)
        }
        WorkerCommand::WriteFile {
            root,
            path,
            overwrite,
            length,
        } => {
            let bytes = mj_core::archive::read_session_file_input(std::io::stdin().lock())
                .map_err(export_error)?;
            if bytes.len() != length {
                return Err(export_error(SessionExportError::Refused(format!(
                    "incomplete file upload: expected {length} bytes, received {}",
                    bytes.len()
                ))));
            }
            mj_core::archive::write_session_file(&root, &path, &bytes, overwrite)
                .map_err(export_error)
        }
        WorkerCommand::PushBranch {
            repository, branch, ..
        } => {
            let pushed =
                mj_core::archive::push_branch(&mj_core::archive::SystemGit, &repository, &branch)
                    .map_err(push_error)?;
            println!("{}", serde_json::to_string(&pushed)?);
            Ok(())
        }
    }
}

/// A precondition an export could not meet, carried out of `run_command` so
/// `main` can answer with [`EXPORT_REFUSED_EXIT_CODE`].
#[derive(Debug)]
struct ExportRefused(String);

impl std::fmt::Display for ExportRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExportRefused {}

fn export_error(error: SessionExportError) -> anyhow::Error {
    match error {
        SessionExportError::Refused(reason) => anyhow::Error::new(ExportRefused(reason)),
        SessionExportError::Failed(error) => error,
    }
}

fn push_error(error: PushBranchError) -> anyhow::Error {
    match error {
        refusal @ (PushBranchError::NoRemote | PushBranchError::InvalidBranch(_)) => {
            anyhow::Error::new(ExportRefused(refusal.to_string()))
        }
        PushBranchError::Failed(error) => error,
    }
}

/// Write an export payload to standard output unchanged. A diff and a file are
/// bytes the caller reassembles, so nothing may add or trim a newline.
fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(bytes)
        .context("write to standard output")?;
    stdout.flush().context("flush standard output")
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
    fn export_subcommands_parse_the_arguments_the_controller_sends() {
        let cli = Cli::try_parse_from([
            "hel",
            "worker",
            "diff",
            "--repository",
            "/workspace/app",
            "--branch",
            "mj/session-1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Worker(WorkerArgs {
                command: WorkerCommand::Diff {
                    repository,
                    base: None,
                    branch: Some(branch),
                },
            }) if repository == Path::new("/workspace/app") && branch == "mj/session-1"
        ));

        let cli = Cli::try_parse_from([
            "hel",
            "worker",
            "read-file",
            "--root",
            "/workspace",
            "--path",
            "app/README.md",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Worker(WorkerArgs {
                command: WorkerCommand::ReadFile { root, path },
            }) if root == Path::new("/workspace") && path == Path::new("app/README.md")
        ));

        let cli = Cli::try_parse_from([
            "hel",
            "worker",
            "push-branch",
            "--root",
            "/worker/session",
            "--repository",
            "/workspace/app",
            "--branch",
            "review/one",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Worker(WorkerArgs {
                command: WorkerCommand::PushBranch { root, repository, branch },
            }) if root == Path::new("/worker/session") && repository == Path::new("/workspace/app") && branch == "review/one"
        ));
    }

    /// A precondition failure leaves its reason on the process, not a generic
    /// error: the daemon maps this exit code to a 409 carrying that reason.
    #[test]
    fn an_export_precondition_failure_carries_the_refusal_exit_code() {
        let root = tempfile::tempdir().unwrap();
        let error = export_error(
            mj_core::archive::read_session_file(root.path(), Path::new("../secret.txt"))
                .unwrap_err(),
        );
        let refusal = error
            .downcast_ref::<ExportRefused>()
            .expect("a path leaving the workspace is a refusal");
        assert!(
            refusal.to_string().contains(".."),
            "the refusal names the reason: {refusal}"
        );
        assert_eq!(EXPORT_REFUSED_EXIT_CODE, 3);

        let missing_remote = push_error(PushBranchError::NoRemote);
        assert_eq!(
            missing_remote
                .downcast_ref::<ExportRefused>()
                .map(ToString::to_string),
            Some("no push remote configured".to_owned())
        );

        // A push that actually ran and failed is not a refusal; it stays an
        // ordinary error so the daemon reports it as a failure.
        let failed = push_error(PushBranchError::Failed(anyhow::anyhow!("git exploded")));
        assert!(failed.downcast_ref::<ExportRefused>().is_none());
    }

    /// A worker that predates these subcommands answers a usage failure, which
    /// is how the controller tells "too old" from "the export failed".
    #[test]
    fn an_unknown_worker_subcommand_is_a_clap_usage_failure() {
        let error = Cli::try_parse_from(["hel", "worker", "diff-not-a-command"]).unwrap_err();
        assert_eq!(error.exit_code(), 2);
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
    #[test]
    fn file_injection_streams_large_stdin_while_draining_worker_output() {
        use mj_core::targets::{CancellableProcessExecutor, CommandExecutor, CommandSpec};
        const CHILD_ROOT: &str = "MJ_TEST_FILE_INPUT_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            // A noisy worker must not deadlock the controller's full stdin.
            write_stdout(&vec![b'x'; 128 * 1024]).unwrap();
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run_command(Command::Worker(WorkerArgs {
                    command: WorkerCommand::WriteFile {
                        root: root.into(),
                        path: PathBuf::from("nested/input.bin"),
                        overwrite: false,
                        length: 512 * 1024,
                    },
                })))
                .unwrap();
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let name = format!(
            "{}::file_injection_streams_large_stdin_while_draining_worker_output",
            module_path!()
                .strip_prefix("mj_worker::")
                .unwrap_or(module_path!())
        );
        let mut command = CommandSpec::new(
            std::env::current_exe().unwrap().to_string_lossy(),
            ["--exact", &name, "--nocapture"],
        )
        .with_sensitive_stdin(bytes.clone());
        command
            .env
            .insert(CHILD_ROOT.into(), root.path().to_string_lossy().into());
        let output = CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(30))
            .execute(&command)
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() >= 128 * 1024);
        assert_eq!(
            std::fs::read(root.path().join("nested/input.bin")).unwrap(),
            bytes
        );
        let interrupted = tempfile::tempdir().unwrap();
        command.env.insert(
            CHILD_ROOT.into(),
            interrupted.path().to_string_lossy().into(),
        );
        let truncated = command.with_sensitive_stdin(vec![0; 128 * 1024]);
        let output = CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(30))
            .execute(&truncated)
            .unwrap();
        assert_ne!(output.status, 0, "premature EOF must fail");
        assert!(
            !interrupted.path().join("nested/input.bin").exists(),
            "an interrupted upload must not publish a partial file"
        );
    }
}

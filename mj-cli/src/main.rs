//! Hel: a session control plane for ACP coding agents.
//!
//! This file owns the command-line surface and the one-shot subcommands. The
//! long-running surfaces live beside it: [`dashboard`] drives the terminal UI,
//! [`server`] implements the daemon-owned phone control, [`pollers`] the background
//! feeds both of them read, and [`import`] session adoption.

mod daemon;
mod dashboard;
#[cfg(all(
    feature = "desktop-app",
    any(
        target_os = "macos",
        target_os = "windows",
        all(target_os = "linux", target_env = "gnu")
    )
))]
mod desktop;
mod import;
mod logging;
mod pollers;
mod server;
mod workspace_selector;

#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hel::hel_config::{HelConfig, config_path};
use hel::hel_controller::Controller;
use hel::hel_setup::{SetupOutcome, run_setup_dialog};
#[cfg(test)]
use hel::hel_targets::ProcessExecutor;
use hel::hel_worker_runtime::{
    AcpSupervisorSpec, WorkerLaunchConfig, lead_process_group, proxy, run_acp_supervisor,
    run_daemon,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::dashboard::{DashboardExit, run_dashboard_for_workspace};
use crate::import::{ImportArgs, import};

#[derive(Debug, Parser)]
#[command(name = "mj", version, about = "ACP session control plane")]
struct Cli {
    /// Select a workspace by name for workspace-scoped commands.
    #[arg(long, global = true)]
    workspace: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the workspace selector even when Mjolnir could auto-attach.
    Workspaces,
    /// Open the web viewer in a native desktop window.
    #[cfg(all(
        feature = "desktop-app",
        any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", target_env = "gnu")
        )
    ))]
    App,
    /// Inspect or control the persistent per-user daemon.
    Daemon(DaemonArgs),
    /// Internal persistent controller process.
    #[command(hide = true)]
    DaemonRun,
    /// Internal target-side worker commands.
    #[command(hide = true)]
    Worker(WorkerArgs),
    /// Internal controller-side local Git broker.
    #[command(hide = true)]
    Broker(BrokerArgs),
    /// Diagnose platform and configuration prerequisites.
    Doctor(DoctorArgs),
    /// Discover local agent homes and create an initial Mjolnir configuration.
    Setup(SetupArgs),
    /// Adopt a native coding-agent session as a stopped Mjolnir session.
    Import(ImportArgs),
    /// Find, adopt, or explicitly destroy managed workers missing from state.
    Recover(RecoverArgs),
    /// Create a verified recovery copy for an active session.
    Checkpoint(CheckpointArgs),
    /// Run a harness login for a profile so live sessions pick up fresh credentials.
    Login(LoginArgs),
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Show daemon PID, version, start time, and client count.
    Status,
    /// Gracefully stop the daemon. Detached workers keep running.
    Stop,
    /// Gracefully replace the daemon with this Mjolnir build.
    Restart,
}

#[derive(Debug, Args)]
struct CheckpointArgs {
    #[arg(long)]
    session: String,
}

#[derive(Debug, Args)]
struct LoginArgs {
    /// Profile to authenticate. Optional when exactly one profile exists.
    #[arg(long)]
    profile: Option<String>,
}

#[derive(Debug, Args)]
struct RecoverArgs {
    #[command(subcommand)]
    command: RecoverCommand,
}

#[derive(Debug, Subcommand)]
enum RecoverCommand {
    /// List managed worker resources not present in controller state.
    Scan {
        #[arg(long)]
        json: bool,
    },
    /// Probe a managed worker and add it back to controller state.
    Adopt {
        #[arg(long)]
        session: String,
        #[arg(long)]
        target: String,
        /// Required only for current-v1 workers created before ownership markers.
        #[arg(long)]
        profile: Option<String>,
        /// Required only for current-v1 workers created before ownership markers.
        #[arg(long)]
        bundle: Option<String>,
    },
    /// Destroy an untracked managed resource after exact-ID confirmation.
    Destroy {
        #[arg(long)]
        session: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        confirm: String,
    },
}

#[derive(Debug, Args)]
struct BrokerArgs {
    #[arg(long)]
    spec: PathBuf,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Emit a machine-readable array of prerequisite checks.
    #[arg(long)]
    json: bool,
    /// Run disposable container smoke tests where supported.
    #[arg(long)]
    smoke: bool,
}

#[derive(Debug, Args)]
struct SetupArgs {
    #[command(subcommand)]
    command: Option<SetupCommand>,
}

#[derive(Debug, Subcommand)]
enum SetupCommand {
    /// Print coding-agent instructions for preparing a host.
    Instructions {
        #[arg(long, value_enum)]
        platform: SetupPlatform,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SetupPlatform {
    Linux,
    Macos,
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

/// Record why a worker died where the controller can find it. The daemon's
/// stdout/stderr go to worker.log; this file is the structured summary read
/// by `Controller` diagnosis when a worker becomes unreachable.
fn write_worker_exit_record(root: &std::path::Path, reason: &str) {
    // Session teardown removes the worker root; recreating it here would
    // resurrect a closed session's state directory.
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

/// Capture panics as last words too; the default hook then prints the
/// backtrace to stderr, which the launch redirect lands in worker.log.
fn install_worker_last_words(root: &std::path::Path) {
    let root = root.to_path_buf();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_worker_exit_record(&root, &format!("panic: {info}"));
        default_hook(info);
    }));
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_user_process = !matches!(&cli.command, Some(Command::Worker(_) | Command::Broker(_)));
    let log = if is_user_process {
        Some(logging::ControllerLog::start(command_name(
            cli.command.as_ref(),
        ))?)
    } else {
        logging::start_stderr()?;
        None
    };
    install_panic_logging();
    let result = run(cli);
    if let Err(error) = &result {
        tracing::error!(error = format!("{error:#}"), "Mjolnir exited with an error");
    }
    if result.is_ok() {
        tracing::info!("Mjolnir stopped");
    }
    drop(log);
    result
}

fn run(cli: Cli) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime")?;
    let result = runtime.block_on(run_command(cli.command, cli.workspace));
    if matches!(
        result,
        Ok(DashboardExit::Detached | DashboardExit::Interrupted)
    ) {
        // A dashboard has already drained durable mutations and restored its
        // terminal. Do not let disposable blocking reads delay process exit.
        shutdown_dashboard_runtime(runtime);
        if matches!(result, Ok(DashboardExit::Detached)) {
            println!(
                "Active sessions will continue working; Mjolnir will reattach to them on your next invocation."
            );
        }
    } else {
        drop(runtime);
    }
    result.map(|_| ())
}

fn install_panic_logging() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "Mjolnir panicked");
        default_hook(info);
    }));
}

fn command_name(command: Option<&Command>) -> &'static str {
    match command {
        None => "dashboard",
        Some(Command::Workspaces) => "workspaces",
        #[cfg(all(
            feature = "desktop-app",
            any(
                target_os = "macos",
                target_os = "windows",
                all(target_os = "linux", target_env = "gnu")
            )
        ))]
        Some(Command::App) => "app",
        Some(Command::Daemon(_)) => "daemon",
        Some(Command::DaemonRun) => "daemon-run",
        Some(Command::Doctor(_)) => "doctor",
        Some(Command::Setup(_)) => "setup",
        Some(Command::Import(_)) => "import",
        Some(Command::Recover(_)) => "recover",
        Some(Command::Checkpoint(_)) => "checkpoint",
        Some(Command::Login(_)) => "login",
        Some(Command::Worker(_)) => "worker",
        Some(Command::Broker(_)) => "broker",
    }
}

fn shutdown_dashboard_runtime(runtime: tokio::runtime::Runtime) {
    runtime.shutdown_background();
}

async fn run_command(
    command: Option<Command>,
    requested_workspace: Option<String>,
) -> Result<DashboardExit> {
    match command {
        None => run_workspace_dashboard(requested_workspace.as_deref(), false, None).await,
        Some(Command::Workspaces) => {
            run_workspace_dashboard(requested_workspace.as_deref(), true, None).await
        }
        #[cfg(all(
            feature = "desktop-app",
            any(
                target_os = "macos",
                target_os = "windows",
                all(target_os = "linux", target_env = "gnu")
            )
        ))]
        Some(Command::App) => desktop::run_desktop_app()
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Daemon(args)) => daemon_command(args).await.map(|()| DashboardExit::Normal),
        Some(Command::DaemonRun) => daemon::run_daemon_process()
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Worker(args)) => match args.command {
            WorkerCommand::Run { root, config } => {
                lead_process_group();
                install_worker_last_words(&root);
                let result = run_daemon(root.clone(), WorkerLaunchConfig::read(&config)?).await;
                if let Err(error) = &result {
                    write_worker_exit_record(&root, &format!("{error:#}"));
                }
                result
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
        .map(|()| DashboardExit::Normal),
        Some(Command::Broker(args)) => hel::hel_git_proxy::run_broker(&args.spec)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Doctor(args)) => doctor(args).map(|()| DashboardExit::Normal),
        Some(Command::Setup(args)) => setup(args).map(|()| DashboardExit::Normal),
        Some(Command::Import(args)) => {
            let workspace_id = resolve_store_workspace(requested_workspace.as_deref()).await?;
            tokio::task::spawn_blocking(move || import(args, &workspace_id))
                .await
                .context("import task panicked")??;
            Ok(DashboardExit::Normal)
        }
        Some(Command::Recover(args)) => recover(args).await.map(|()| DashboardExit::Normal),
        Some(Command::Checkpoint(args)) => {
            let checkpoint = daemon::connect_or_start()
                .await?
                .checkpoint_session(args.session.clone())
                .await?;
            println!(
                "saved recovery copy for {} at event {}",
                args.session, checkpoint.event_frontier
            );
            Ok(DashboardExit::Normal)
        }
        Some(Command::Login(args)) => login(args).await.map(|()| DashboardExit::Normal),
    }
}

async fn run_workspace_dashboard(
    requested_workspace: Option<&str>,
    force_selector: bool,
    fallback_workspace: Option<String>,
) -> Result<DashboardExit> {
    let mut daemon = daemon::connect_or_start().await?;
    let mut workspaces = daemon.list_workspaces().await?;
    let selected = if let Some(requested) = requested_workspace.filter(|_| !force_selector) {
        workspaces
            .iter()
            .find(|candidate| {
                candidate.workspace.name.to_lowercase() == requested.trim().to_lowercase()
            })
            .map(|candidate| candidate.workspace.id.clone())
            .with_context(|| format!("unknown workspace {requested:?}"))?
    } else if !force_selector && workspaces.len() == 1 && workspaces[0].attached_pids.is_empty() {
        workspaces[0].workspace.id.clone()
    } else if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        match workspaces.as_slice() {
            [workspace] => workspace.workspace.id.clone(),
            [] => bail!("no workspace exists; run `mj` in a terminal to create one"),
            _ => bail!("several workspaces exist; pass `--workspace NAME`"),
        }
    } else {
        loop {
            let suggested = suggested_workspace_name(&workspaces)?;
            let mut selector_entries = Vec::with_capacity(workspaces.len());
            for listing in &workspaces {
                let snapshot = daemon.snapshot(listing.workspace.id.clone()).await?;
                selector_entries.push(workspace_selector::SelectorWorkspace {
                    listing: listing.clone(),
                    snapshot,
                });
            }
            match workspace_selector::select_workspace(&selector_entries, &suggested)? {
                workspace_selector::SelectorOutcome::Select(workspace_id) => break workspace_id,
                workspace_selector::SelectorOutcome::Create(name) => {
                    let workspace = daemon.create_workspace(name).await?;
                    break workspace.id;
                }
                workspace_selector::SelectorOutcome::Rename { workspace_id, name } => {
                    daemon.rename_workspace(workspace_id, name).await?;
                    workspaces = daemon.list_workspaces().await?;
                }
                workspace_selector::SelectorOutcome::Delete(workspace_id) => {
                    daemon.delete_workspace(workspace_id).await?;
                    workspaces = daemon.list_workspaces().await?;
                }
                workspace_selector::SelectorOutcome::RecoverDraft(draft_id) => {
                    daemon.recover_draft(draft_id).await?;
                    workspaces = daemon.list_workspaces().await?;
                }
                workspace_selector::SelectorOutcome::Cancel => {
                    if let Some(workspace_id) = &fallback_workspace {
                        break workspace_id.clone();
                    }
                    return Ok(DashboardExit::Normal);
                }
            }
        }
    };

    let client_id = format!(
        "tui-{}-{}",
        std::process::id(),
        hel::hel_workspace::new_workspace_id()?
    );
    daemon
        .attach(selected.clone(), client_id.clone(), std::process::id())
        .await?;
    let attachment_cancellation = tokio_util::sync::CancellationToken::new();
    let attachment_task = daemon::maintain_attachment(
        selected.clone(),
        client_id.clone(),
        std::process::id(),
        attachment_cancellation.clone(),
    );
    let result = run_dashboard_for_workspace(&selected, &client_id).await;
    attachment_cancellation.cancel();
    if let Err(error) = attachment_task.await {
        tracing::warn!(%error, "workspace attachment task failed");
    }
    match daemon::connect_existing().await {
        Ok(mut current_daemon) => {
            if let Err(error) = current_daemon.detach(client_id).await {
                tracing::warn!(%error, "could not detach dashboard from workspace");
            }
        }
        Err(error) => tracing::warn!(%error, "daemon unavailable while dashboard detached"),
    }
    if matches!(result, Ok(DashboardExit::WorkspacePicker)) {
        Box::pin(run_workspace_dashboard(None, true, Some(selected))).await
    } else {
        result
    }
}

async fn resolve_store_workspace(requested: Option<&str>) -> Result<String> {
    let mut daemon = daemon::connect_or_start().await?;
    let workspaces = daemon.list_workspaces().await?;
    if let Some(requested) = requested {
        return workspaces
            .iter()
            .find(|candidate| {
                candidate.workspace.name.to_lowercase() == requested.trim().to_lowercase()
            })
            .map(|candidate| candidate.workspace.id.clone())
            .with_context(|| format!("unknown workspace {requested:?}"));
    }
    match workspaces.as_slice() {
        [workspace] => Ok(workspace.workspace.id.clone()),
        [] => bail!("no workspace exists; run `mj` to create one"),
        _ => bail!("several workspaces exist; pass `--workspace NAME`"),
    }
}

fn suggested_workspace_name(workspaces: &[daemon::WorkspaceListing]) -> Result<String> {
    let base = std::env::current_dir()
        .context("read current directory for workspace name")?
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("workspace")
        .trim()
        .chars()
        .take(64)
        .collect::<String>();
    let names = workspaces
        .iter()
        .map(|candidate| candidate.workspace.name.to_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    if !names.contains(&base.to_lowercase()) {
        return Ok(base);
    }
    for number in 2..=10_000 {
        let suffix = format!("-{number}");
        let prefix_length = 64_usize.saturating_sub(suffix.chars().count());
        let candidate = format!(
            "{}{}",
            base.chars().take(prefix_length).collect::<String>(),
            suffix
        );
        if !names.contains(&candidate.to_lowercase()) {
            return Ok(candidate);
        }
    }
    Ok("workspace-1".to_owned())
}

async fn daemon_command(args: DaemonArgs) -> Result<()> {
    match args.command {
        DaemonCommand::Status => {
            let mut daemon = daemon::connect_management()
                .await
                .context("Mjolnir daemon is not running")?;
            let status = daemon.status().await?;
            println!(
                "Mjolnir daemon {} (version {}) started {}; {} attached client{}; web viewer {}",
                status.pid,
                status.build_version,
                status.started_at,
                status.attached_clients,
                if status.attached_clients == 1 {
                    ""
                } else {
                    "s"
                },
                status.phone_status
            );
            if daemon.protocol_version() != daemon::PROTOCOL_VERSION {
                println!(
                    "The daemon speaks protocol {} while this build speaks {}; \
                     status/stop/restart work, and other commands will replace it on next use.",
                    daemon.protocol_version(),
                    daemon::PROTOCOL_VERSION
                );
            }
        }
        DaemonCommand::Stop => {
            let mut daemon = daemon::connect_management()
                .await
                .context("Mjolnir daemon is not running")?;
            daemon.stop().await?;
            println!("Mjolnir daemon is stopping; detached workers remain active.");
        }
        DaemonCommand::Restart => {
            if let Ok(daemon) = daemon::connect_management().await {
                daemon.stop_and_wait().await?;
            }
            let mut daemon = daemon::connect_or_start().await?;
            let status = daemon.status().await?;
            println!("Mjolnir daemon restarted as PID {}.", status.pid);
        }
    }
    Ok(())
}

/// Run the harness's own interactive login against a profile's canonical home.
/// Hel never sees the credential contents; it compares fingerprints before and
/// after so it can tell the operator whether anything changed.
async fn login(args: LoginArgs) -> Result<()> {
    let controller = Controller::load()?;
    let profile_id = resolve_login_profile(&controller.config, args.profile.as_deref())?;
    let profile = controller
        .config
        .profiles
        .get(&profile_id)
        .with_context(|| {
            format!(
                "unknown profile {profile_id:?}; configured profiles: {}",
                profile_ids(&controller.config)
            )
        })?;
    let marker = hel::hel_setup::harness_authentication_marker(profile.kind, &profile.home);
    let (before, _) = hel::hel_credentials::read_credential_file(profile.kind, &marker)?;
    let (program, arguments) = hel::hel_credentials::login_command(profile);

    println!(
        "Running `{program} {}` against {}.",
        arguments.join(" "),
        profile.home.display()
    );
    let status = tokio::process::Command::new(&program)
        .args(&arguments)
        .envs(&profile.environment)
        .env(profile.home_env(), &profile.home)
        .status()
        .await
        .with_context(|| {
            format!(
                "run `{program} {}` for profile {profile_id}",
                arguments.join(" ")
            )
        })?;

    let (after, _) = hel::hel_credentials::read_credential_file(profile.kind, &marker)?;
    if after.present && after.fingerprint != before.fingerprint {
        println!(
            "Credentials updated for profile {profile_id}. Live sessions pick them up within about a minute while the Mjolnir daemon is running."
        );
    } else {
        println!("Credentials for profile {profile_id} are unchanged.");
    }
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn profile_ids(config: &HelConfig) -> String {
    config
        .profiles
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

fn resolve_login_profile(config: &HelConfig, requested: Option<&str>) -> Result<String> {
    if let Some(profile) = requested {
        return Ok(profile.to_owned());
    }
    let mut profiles = config.profiles.keys();
    match (profiles.next(), profiles.next()) {
        (Some(only), None) => Ok(only.clone()),
        (Some(_), Some(_)) => bail!(
            "several profiles are configured; pass --profile with one of: {}",
            profile_ids(config)
        ),
        (None, _) => bail!("no harness profiles are configured; run `mj setup` first"),
    }
}

async fn recover(args: RecoverArgs) -> Result<()> {
    let mut daemon = daemon::connect_or_start().await?;
    match args.command {
        RecoverCommand::Scan { json } => {
            let scan = daemon.scan_recovery().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&scan)?);
            } else {
                for candidate in &scan.candidates {
                    let metadata = if candidate.ownership.is_some() {
                        "ownership verified"
                    } else {
                        "v1 resource; profile and bundle unknown"
                    };
                    println!(
                        "{}\t{}\t{}",
                        candidate.session_id, candidate.target_template_id, metadata
                    );
                }
                for warning in &scan.warnings {
                    eprintln!("warning: {warning}");
                }
            }
            Ok(())
        }
        RecoverCommand::Adopt {
            session,
            target,
            profile,
            bundle,
        } => {
            daemon
                .adopt_recovery(session.clone(), target, profile, bundle)
                .await?;
            println!("adopted worker {session}");
            Ok(())
        }
        RecoverCommand::Destroy {
            session,
            target,
            confirm,
        } => {
            daemon
                .destroy_recovery(session.clone(), target, confirm)
                .await?;
            println!("destroyed orphan worker resource {session}");
            Ok(())
        }
    }
}

fn setup(args: SetupArgs) -> Result<()> {
    match args.command {
        Some(SetupCommand::Instructions { platform }) => {
            let platform = match platform {
                SetupPlatform::Linux => hel::hel_doctor::InstructionsPlatform::Linux,
                SetupPlatform::Macos => hel::hel_doctor::InstructionsPlatform::Macos,
            };
            print!("{}", hel::hel_doctor::setup_instructions(platform));
            Ok(())
        }
        None => match run_setup_dialog(&config_path())? {
            SetupOutcome::Written | SetupOutcome::Cancelled => Ok(()),
        },
    }
}

fn doctor(args: DoctorArgs) -> Result<()> {
    let checks = hel::hel_doctor::run_current(hel::hel_doctor::DoctorOptions { smoke: args.smoke });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        hel::hel_doctor::render_human(&checks, &mut io::stdout())?;
    }
    if hel::hel_doctor::all_ready(&checks) {
        Ok(())
    } else {
        Err(doctor_failure())
    }
}

fn doctor_failure() -> anyhow::Error {
    anyhow::anyhow!(
        "Mjolnir has fixable prerequisites; run `mj doctor --json` and follow its remediations."
    )
}

/// The prefix every message uses when it names a session, so notices stay
/// readable without losing which session they are about.
pub(crate) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub(crate) struct TerminalGuard {
    pub(crate) terminal: Terminal<CrosstermBackend<io::Stdout>>,
    keyboard_enhancement: bool,
}

impl TerminalGuard {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut stdout = io::stdout();
        // Legacy terminal input encodes Ctrl+I as the same byte as Tab. Ask
        // capable terminals to report them distinctly so both bindings work.
        let keyboard_enhancement = matches!(
            crossterm::terminal::supports_keyboard_enhancement(),
            Ok(true)
        );
        if keyboard_enhancement {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .context("enable unambiguous terminal key reporting")?;
        }
        // Capture stays on for every surface: the app owns wheel scrolling
        // because terminal scrollback repaints whole TUI frames and is
        // unusably slow on long sessions, and pane-scoped selection needs the
        // button and drag reports too. Shift+drag still reaches the
        // terminal's own selection.
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .context("enter alternate screen and enable terminal input modes")?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self {
            terminal,
            keyboard_enhancement,
        })
    }

    /// Hands `text` to the terminal's own clipboard with OSC 52.
    ///
    /// This is the path that works over SSH and inside multiplexers, where
    /// the desktop clipboard the process can reach is the wrong machine's.
    pub(crate) fn copy_to_terminal_clipboard(&mut self, text: &str) -> Result<()> {
        execute!(
            self.terminal.backend_mut(),
            CopyToClipboard::to_clipboard_from(text)
        )
        .context("copy selection to the terminal clipboard")
    }

    pub(crate) fn suspend(&mut self) -> Result<()> {
        if self.keyboard_enhancement {
            execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags)
                .context("restore terminal key reporting for setup")?;
        }
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .context("disable terminal input modes and leave alternate screen for setup")?;
        disable_raw_mode().context("disable terminal raw mode for setup")?;
        self.terminal
            .show_cursor()
            .context("show cursor for setup")?;
        Ok(())
    }

    pub(crate) fn resume(&mut self) -> Result<()> {
        enable_raw_mode().context("re-enable terminal raw mode after setup")?;
        if self.keyboard_enhancement {
            execute!(
                self.terminal.backend_mut(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .context("re-enable unambiguous terminal key reporting after setup")?;
        }
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .context("re-enter alternate screen and enable terminal input modes after setup")?;
        self.terminal
            .clear()
            .context("clear dashboard after setup")?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.keyboard_enhancement
            && let Err(error) = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags)
        {
            tracing::warn!(%error, "could not restore terminal keyboard enhancement flags");
        }
        if let Err(error) = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        ) {
            tracing::warn!(%error, "could not restore terminal screen and input modes");
        }
        if let Err(error) = disable_raw_mode() {
            tracing::warn!(%error, "could not disable terminal raw mode");
        }
        if let Err(error) = self.terminal.show_cursor() {
            tracing::warn!(%error, "could not show terminal cursor");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_state::{HelState, SessionRecord, SessionState};

    #[test]
    fn dashboard_runtime_does_not_wait_for_disposable_blocking_work() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();

        let started = std::time::Instant::now();
        shutdown_dashboard_runtime(runtime);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(250),
            "fast dashboard shutdown waited {:?}",
            started.elapsed()
        );
        release_tx.send(()).unwrap();
    }

    /// The controller streams a checkpoint spec by asking the worker to read
    /// `--spec -`, so that dash has to survive argument parsing as a value.
    #[test]
    fn export_checkpoint_accepts_a_dash_for_a_streamed_spec() {
        let cli = Cli::try_parse_from(["hel", "worker", "export-checkpoint", "--spec", "-"])
            .expect("a streamed spec is a valid export argument");
        let Some(Command::Worker(WorkerArgs {
            command: WorkerCommand::ExportCheckpoint { spec },
        })) = cli.command
        else {
            panic!("export-checkpoint did not parse as a worker command");
        };
        assert_eq!(spec, PathBuf::from("-"));
    }

    #[test]
    fn two_phase_checkpoint_worker_commands_parse_without_file_arguments() {
        for (name, expected) in [
            ("capture-checkpoint", "capture"),
            ("pack-checkpoint", "pack"),
        ] {
            let cli = Cli::try_parse_from(["hel", "worker", name]).unwrap();
            let Some(Command::Worker(WorkerArgs { command })) = cli.command else {
                panic!("{name} did not parse as a worker command");
            };
            assert!(
                matches!(
                    (command, expected),
                    (WorkerCommand::CaptureCheckpoint, "capture")
                        | (WorkerCommand::PackCheckpoint, "pack")
                ),
                "{name} parsed as the wrong worker command"
            );
        }
    }

    #[test]
    fn login_uses_the_sole_profile_and_otherwise_demands_a_choice() {
        let mut config = HelConfig::default();
        assert!(resolve_login_profile(&config, None).is_err());

        config.profiles.insert(
            "work".into(),
            hel::hel_config::HarnessProfile {
                kind: hel::hel_config::HarnessKind::Claude,
                home: PathBuf::from("/home/user/.claude"),
                executable: None,
                environment: Default::default(),
                context_window_bytes: None,
            },
        );
        assert_eq!(resolve_login_profile(&config, None).unwrap(), "work");

        config
            .profiles
            .insert("personal".into(), config.profiles["work"].clone());
        let error = resolve_login_profile(&config, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("personal, work"), "{error}");
        assert_eq!(
            resolve_login_profile(&config, Some("personal")).unwrap(),
            "personal"
        );
    }

    #[test]
    fn short_session_ids_are_safe() {
        assert_eq!(short_id("0123456789"), "01234567");
        assert_eq!(short_id("tiny"), "tiny");
    }

    #[test]
    fn cli_name_and_worker_shape_are_stable() {
        use clap::CommandFactory;
        let command = Cli::command();
        assert_eq!(command.get_name(), "mj");
        assert!(
            command
                .get_subcommands()
                .any(|sub| sub.get_name() == "worker")
        );
        assert!(
            command
                .get_subcommands()
                .any(|sub| sub.get_name() == "setup")
        );
        let login = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "login")
            .expect("hel login is a visible command");
        assert!(!login.is_hide_set());
    }

    #[test]
    fn doctor_json_and_setup_instructions_are_parseable() {
        let doctor = Cli::try_parse_from(["hel", "doctor", "--json"]).unwrap();
        assert!(matches!(
            doctor.command,
            Some(Command::Doctor(DoctorArgs {
                json: true,
                smoke: false
            }))
        ));

        let setup =
            Cli::try_parse_from(["hel", "setup", "instructions", "--platform", "linux"]).unwrap();
        assert!(matches!(
            setup.command,
            Some(Command::Setup(SetupArgs {
                command: Some(SetupCommand::Instructions {
                    platform: SetupPlatform::Linux
                })
            }))
        ));
    }

    #[test]
    fn doctor_failure_uses_mjolnir_product_wording() {
        let message = doctor_failure().to_string();
        assert!(message.contains("Mjolnir"));
        assert!(!message.contains("Hel"));
        assert!(message.contains("mj doctor --json"));
    }

    #[test]
    fn failed_archive_removal_retains_session_metadata_for_retry() {
        let directory = tempfile::tempdir().unwrap();
        let archive_path = directory.path().join("checkpoint.hel.zip");
        std::fs::create_dir(&archive_path).unwrap();
        let session_id = "1123456789abcdef0123456789abcdef";
        let mut state = HelState::default();
        state.sessions.insert(
            session_id.into(),
            SessionRecord {
                workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                archived: false,
                container_cpus: None,
                container_memory: None,
                id: session_id.into(),
                title: "stopped".into(),
                harness_kind: hel::hel_config::HarnessKind::Codex,
                last_profile: "codex".into(),
                bundle_id: "project".into(),
                project_directory: None,
                managed_worktree: None,
                target_template_id: "podman".into(),
                resource_allocation: None,
                additional_mounts: Vec::new(),
                state: SessionState::Stopped,
                target: None,
                native_session_id: Some("native-session".into()),
                acp_session_title: None,
                session_title_override: None,
                created_at: "2026-08-12T00:00:00Z".into(),
                updated_at: "2026-08-12T00:00:00Z".into(),
                viewed_through_event_ordinal: 0,
                draft_input: String::new(),
                last_error: None,
                last_checkpoint_error: None,
                checkpoint: Some(hel::hel_state::CheckpointMetadata {
                    archive_path,
                    sha256: "a".repeat(64),
                    created_at: "2026-08-12T00:00:00Z".into(),
                    event_frontier: 7,
                }),
            },
        );
        let mut controller = Controller {
            config: HelConfig::default(),
            state,
        };

        assert!(
            controller
                .destroy_session_controlled(session_id, &ProcessExecutor)
                .is_err()
        );
        assert!(controller.state.sessions.contains_key(session_id));
    }
}

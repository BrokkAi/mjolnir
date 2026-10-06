//! Hel: a session control plane for ACP coding agents.
//!
//! This file owns the command-line surface and the one-shot subcommands. The
//! long-running surfaces live beside it: [`dashboard`] drives the terminal UI,
//! [`server`] implements the daemon-owned phone control, [`pollers`] the background
//! feeds both of them read, and [`import`] session adoption.

mod acp;
mod api_client;
mod api_commands;
mod daemon;
mod dashboard;
mod desktop;
mod go;
mod hints;
mod import;
mod logging;
mod pollers;
mod session_presentation;
mod splash;

#[cfg(test)]
mod test_support;

use anyhow::{Context, Result, bail, ensure};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use mj_core::config::{Config, config_path};
use mj_core::state::{MoveSelection, MoveSessionRequest, ResumeQueueDisposition};
use std::io::{self, Write};

use mj_controller::controller::Controller;
use mj_controller::setup::run_setup_command;
#[cfg(test)]
use mj_controller::targets::ProcessExecutor;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::dashboard::{DashboardExit, run_dashboard_for_workspace};
use crate::import::{ImportArgs, import};

#[derive(Debug, Parser)]
#[command(name = "mj", version, about = "ACP session control plane")]
struct Cli {
    /// Isolate configuration, database, daemon, and logs under
    /// `instances/<name>`. This is a different `mj` world, not a harness
    /// profile; explicit `MJ_CONFIG_DIR`/`MJ_DATA_DIR` still take precedence.
    #[arg(
        short = 'i',
        long,
        global = true,
        env = "MJ_INSTANCE",
        value_name = "NAME"
    )]
    instance: Option<String>,
    /// Open the dashboard in this workspace, by name. Given before a command,
    /// it applies to that command when the command works in a workspace.
    #[arg(long, value_name = "NAME")]
    workspace: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
// Parsed once per process, so the size of the largest subcommand's arguments
// costs nothing; the variants' sizes differ by target because their platform
// argument types do.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Work in a folder using remembered account and target settings.
    Go(go::GoArgs),
    /// Open the workspace selector even when Mjolnir could auto-attach, or
    /// list and create workspaces without a terminal.
    Workspaces(WorkspacesArgs),
    /// Open the web viewer in a native desktop window.
    App,
    /// Mint the private launch document consumed by `mj-desktop`.
    #[command(hide = true)]
    DesktopBootstrap,
    /// Inspect or control the persistent per-user daemon.
    Daemon(DaemonArgs),
    /// Internal persistent controller process.
    #[command(hide = true)]
    DaemonRun,
    /// Serve the Agent Client Protocol on standard input and output, running
    /// each session it creates through this daemon.
    Acp(acp::AcpArgs),
    /// Diagnose platform and configuration prerequisites.
    Doctor(DoctorArgs),
    /// Rerun agent and repository discovery, preserving existing configuration.
    Setup(SetupArgs),
    /// Adopt a native coding-agent session as a stopped Mjolnir session.
    Import(ImportArgs),
    /// Find, adopt, or explicitly destroy managed workers missing from state.
    Recover(RecoverArgs),
    /// Create a verified recovery copy for an active session.
    Checkpoint(CheckpointArgs),
    /// Move a session to another configured profile and/or target.
    Move(MoveArgs),
    /// Inspect stopped sources retained by Move, or explicitly remove one.
    MoveSources {
        /// Session whose retained Move sources should be listed or cleaned up.
        #[arg(long)]
        session: String,
        /// Operation ID of the retained source to delete.
        #[arg(long)]
        cleanup: Option<String>,
        /// Confirm deletion of the retained source and its excluded files.
        #[arg(long)]
        yes: bool,
    },
    /// Run a harness login for a profile so live sessions pick up fresh credentials.
    Login(LoginArgs),
    /// Create a session through the API and print its id.
    New(api_commands::NewArgs),
    /// Send a prompt to a session, optionally waiting for the turn.
    Prompt(api_commands::PromptArgs),
    /// Block until a turn ends and print how it ended.
    Wait(api_commands::WaitArgs),
    /// Page through a session's transcript.
    Transcript(api_commands::TranscriptArgs),
    /// Read recorded token usage and coverage.
    Usage(api_commands::UsageArgs),
    /// Follow durable session events as line-delimited JSON.
    Events(api_commands::EventsArgs),
    /// Atomically upload a file to an idle session workspace.
    PutFile(api_commands::PutFileArgs),
    /// List structured input requests from a session.
    Elicitations(api_commands::ElicitationsArgs),
    /// Respond to a structured input request.
    Respond(api_commands::RespondArgs),
    /// Print a unified diff of a session's work.
    Diff(api_commands::DiffArgs),
    /// Get a session's work out as a patch, a pushed branch, or a git bundle.
    Export(api_commands::ExportArgs),
    /// List the sessions the daemon holds.
    Sessions(api_commands::SessionsArgs),
    /// Save a recovery copy and release the environment for later Resume.
    Suspend(api_commands::SuspendArgs),
    /// Permanently destroy a session, its environment, and recovery archive.
    Destroy(api_commands::DestroyArgs),
    /// Resume a stopped session from its checkpoint.
    Resume(api_commands::ResumeArgs),
    /// Cancel the turn a session is running.
    InterruptTurn(api_commands::SessionArgs),
    /// Start a review of a session's last turn, or show the review it has open.
    Review(api_commands::ReviewArgs),
    /// Stop a session's background task, or all tasks the worker can stop.
    StopTask(api_commands::StopTaskArgs),
    /// Print the API base URL and where its bearer token lives.
    ApiInfo(api_commands::ApiInfoArgs),
    /// Print a valid GitHub App token for an owner or repository.
    GithubToken(api_commands::GithubTokenArgs),
    /// Discover available models and efforts for a profile.
    Models(api_commands::ModelsArgs),
    /// Apply a session configuration setting.
    SetConfig(api_commands::SetConfigArgs),
    /// Removed; kept so the old name says what replaced it.
    #[command(hide = true)]
    Close(RemovedCommandArgs),
    /// Removed; kept so the old name says what replaced it.
    #[command(hide = true)]
    CancelTurn(RemovedCommandArgs),
}

/// `--workspace` for the commands that work in a workspace. It was a global
/// option, so `--help` showed it on every command, including the many where
/// it did nothing (F-16).
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct WorkspaceName {
    /// Workspace to work in, by name. `mj new` and `mj acp` require it.
    /// `mj sessions` and `mj events` show only that workspace when it is
    /// given, and every workspace when it is not. `mj import`, and `mj resume
    /// --wiki` when it imports another tool's session, put the session in it,
    /// and need it when the instance has more than one workspace.
    #[arg(long = "workspace", id = "workspace_name", value_name = "NAME")]
    pub(crate) name: Option<String>,
}

impl WorkspaceName {
    /// The name given with the command, or else the one given before it.
    pub(crate) fn or(&self, before_command: Option<String>) -> Option<String> {
        self.name.clone().or(before_command)
    }
}

/// Whatever was passed to a removed command. It is accepted only so the
/// command can name its replacement instead of failing on its arguments.
#[derive(Debug, Args)]
struct RemovedCommandArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    rest: Vec<String>,
}

/// What a removed command was replaced by, for a command that was removed.
fn replacement_notice(command: Option<&Command>) -> Option<&'static str> {
    match command {
        Some(Command::Close(_)) => Some("`mj close` was replaced by `mj suspend` and `mj destroy`"),
        Some(Command::CancelTurn(_)) => {
            Some("`mj cancel-turn` was replaced by `mj interrupt-turn`")
        }
        _ => None,
    }
}

/// `mj workspaces` on its own opens the workspace manager in the dashboard, as
/// it always has. The subcommands are the non-interactive form a script uses.
#[derive(Debug, Args)]
struct WorkspacesArgs {
    #[command(subcommand)]
    command: Option<WorkspacesCommand>,
}

#[derive(Debug, Subcommand)]
enum WorkspacesCommand {
    /// List the workspaces sessions can be created in.
    List(api_commands::WorkspacesListArgs),
    /// Create a workspace, or select the one that already has the name.
    Create(api_commands::WorkspaceCreateArgs),
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
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("destination")
        .args(["target", "profile"])
        .required(true)
        .multiple(true)
))]
struct MoveArgs {
    /// Inspect the transfer and return its preparation without moving anything.
    #[arg(long)]
    prepare: bool,
    /// Explicitly accept a workspace transfer of at least 1 GB.
    #[arg(long)]
    allow_large_transfer: bool,
    /// Leave an untracked path at the source (repeatable REPOSITORY:PATH).
    #[arg(long = "exclude")]
    exclusions: Vec<String>,
    /// Session to move. The session identity is retained by the operation.
    #[arg(long)]
    session: String,
    /// Destination target template. Omit to retain the current target.
    #[arg(long)]
    target: Option<String>,
    /// Destination profile. Omit to retain the current profile.
    #[arg(long)]
    profile: Option<String>,
    /// What to do with prompts/configuration commands already queued.
    #[arg(long, value_enum)]
    queue: Option<MoveQueue>,
    /// Confirm interruption and run without an interactive prompt.
    #[arg(long)]
    yes: bool,
    /// Print exactly one structured outcome to stdout.
    #[arg(long)]
    json: bool,
    /// Explicitly remove inherited container sizing when moving to a fixed host.
    #[arg(long)]
    clear_resources: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MoveQueue {
    Discard,
    Start,
}

impl From<MoveQueue> for ResumeQueueDisposition {
    fn from(queue: MoveQueue) -> Self {
        match queue {
            MoveQueue::Discard => Self::Discard,
            MoveQueue::Start => Self::Start,
        }
    }
}

#[derive(Debug, Args)]
struct LoginArgs {
    /// Profile to authenticate. Optional when exactly one profile exists.
    #[arg(long)]
    profile: Option<String>,
    /// Claude profiles only: mint a long-lived subscription token with
    /// `claude setup-token` and store it for every session of this profile.
    #[arg(long)]
    setup_token: bool,
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
        /// Print the response as JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Also list workers created by other Mjolnir instances, or by builds
        /// that left no instance stamp.
        #[arg(long)]
        all_instances: bool,
    },
    /// Probe a managed worker and add it back to controller state.
    Adopt {
        /// Session id of the worker, as `mj recover scan` lists it.
        #[arg(long)]
        session: String,
        /// Target the worker runs on, as `mj recover scan` lists it.
        #[arg(long)]
        target: String,
        /// Required only for current-v1 workers created before ownership markers.
        #[arg(long)]
        profile: Option<String>,
        /// Required only for current-v1 workers created before ownership markers.
        #[arg(long)]
        bundle: Option<String>,
        /// Allow adopting a worker another instance created, or one with no
        /// instance stamp.
        #[arg(long)]
        all_instances: bool,
    },
    /// Destroy an untracked managed resource after exact-ID confirmation.
    Destroy {
        /// Session id of the worker, as `mj recover scan` lists it.
        #[arg(long)]
        session: String,
        /// Target the worker runs on, as `mj recover scan` lists it.
        #[arg(long)]
        target: String,
        /// The session id again, to confirm the destruction.
        #[arg(long)]
        confirm: String,
        /// Allow destroying a worker another instance created, or one with no
        /// instance stamp.
        #[arg(long)]
        all_instances: bool,
    },
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
        /// Platform to prepare.
        #[arg(long, value_enum)]
        platform: SetupPlatform,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SetupPlatform {
    Linux,
    Macos,
}

fn main() -> Result<()> {
    mj_controller::server::install_rustls_crypto_provider();
    let cli = Cli::parse();
    if let Some(notice) = replacement_notice(cli.command.as_ref()) {
        // Exit 2, the status clap uses for a usage error, which this is.
        eprintln!("error: {notice}");
        std::process::exit(2);
    }
    // Apply before logging, daemon startup, or any path lookup: everything
    // derives its directories from the instance environment, and the daemon
    // child inherits it. This also covers `daemon-run`, which is this same
    // binary, so a per-instance daemon validates its own environment on boot.
    mj_core::config::apply_instance_flag(cli.instance.as_deref())?;
    let log = Some(logging::ControllerLog::start(
        command_name(cli.command.as_ref()),
        process_kind(cli.command.as_ref()),
    )?);
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
    if matches!(
        cli.command,
        None | Some(Command::Workspaces(WorkspacesArgs { command: None }))
    ) {
        // Interactive dashboard startup is the one place an update may be
        // checked, prompted for, and applied on every launch. The whole flow
        // finishes before daemon startup and the TUI takes over the terminal; a
        // successful upgrade never returns because the process re-execs.
        runtime.block_on(mj_controller::controller::update::check_prompt_and_apply());
    }
    let daemon = matches!(cli.command, Some(Command::DaemonRun));
    let result = runtime.block_on(run_command(cli.command, cli.workspace));
    if daemon {
        shutdown_daemon_runtime(runtime);
    } else if matches!(
        &result,
        Ok(DashboardExit::Detached | DashboardExit::Interrupted | DashboardExit::Restart { .. })
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
        mj_core::runtime::shutdown(
            runtime,
            mj_core::runtime::BlockingWork::Await,
            RUNTIME_SHUTDOWN_GRACE,
        );
    }
    match result? {
        DashboardExit::Restart { target, resume } => resume.restart(&target),
        _ => Ok(()),
    }
}

fn install_panic_logging() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "Mjolnir panicked");
        default_hook(info);
    }));
}

/// Classifies a command for log retention: the persistent daemon and any
/// terminal-owning interactive surface each get their own retained window so
/// short CLI invocations never push a long-running process's log out of it.
fn process_kind(command: Option<&Command>) -> logging::ProcessKind {
    match command {
        Some(Command::DaemonRun) => logging::ProcessKind::Daemon,
        None
        | Some(Command::Go(_))
        | Some(Command::Workspaces(WorkspacesArgs { command: None }))
        | Some(Command::App) => logging::ProcessKind::Tui,
        _ => logging::ProcessKind::Cli,
    }
}

fn command_name(command: Option<&Command>) -> &'static str {
    match command {
        None => "dashboard",
        Some(Command::Go(_)) => "go",
        Some(Command::Workspaces(_)) => "workspaces",
        Some(Command::App) => "app",
        Some(Command::DesktopBootstrap) => "desktop-bootstrap",
        Some(Command::Daemon(_)) => "daemon",
        Some(Command::DaemonRun) => "daemon-run",
        Some(Command::Acp(_)) => "acp",
        Some(Command::Doctor(_)) => "doctor",
        Some(Command::Setup(_)) => "setup",
        Some(Command::Import(_)) => "import",
        Some(Command::Recover(_)) => "recover",
        Some(Command::Checkpoint(_)) => "checkpoint",
        Some(Command::Move(_)) => "move",
        Some(Command::MoveSources { .. }) => "move-sources",
        Some(Command::Login(_)) => "login",
        Some(Command::New(_)) => "new",
        Some(Command::Prompt(_)) => "prompt",
        Some(Command::Wait(_)) => "wait",
        Some(Command::Transcript(_)) => "transcript",
        Some(Command::Usage(_)) => "usage",
        Some(Command::Events(_)) => "events",
        Some(Command::PutFile(_)) => "put-file",
        Some(Command::Elicitations(_)) => "elicitations",
        Some(Command::Respond(_)) => "respond",
        Some(Command::Diff(_)) => "diff",
        Some(Command::Export(_)) => "export",
        Some(Command::Sessions(_)) => "sessions",
        Some(Command::Suspend(_)) => "suspend",
        Some(Command::Destroy(_)) => "destroy",
        Some(Command::Resume(_)) => "resume",
        Some(Command::InterruptTurn(_)) => "interrupt-turn",
        Some(Command::Review(_)) => "review",
        Some(Command::StopTask(_)) => "stop-task",
        Some(Command::ApiInfo(_)) => "api-info",
        Some(Command::GithubToken(_)) => "github-token",
        Some(Command::Models(_)) => "models",
        Some(Command::SetConfig(_)) => "set-config",
        Some(Command::Close(_)) => "close",
        Some(Command::CancelTurn(_)) => "cancel-turn",
    }
}

/// How long runtime shutdown waits for blocking threads that are awaiting
/// runtime work through `mj_core::runtime::block_on`. Those threads must stop
/// polling their timers before the runtime's timer driver goes away, or tokio
/// asserts.
const RUNTIME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the daemon's exit waits for guarded blocking awaits, and then for
/// the rest of the blocking pool.
///
/// By the time the runtime shuts down, the daemon's epilogue has stopped every
/// task it owns and the database writer has committed every accepted write.
/// What still runs on the blocking pool is work no owner waits for, such as
/// the `spawn_blocking` half of a task the epilogue aborted. Waiting for it
/// without a bound kept the old daemon alive, and the next one waiting, for
/// seconds after its epilogue had finished.
const DAEMON_BLOCKING_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The daemon's runtime shutdown: bounded, and it names what it leaves behind.
fn shutdown_daemon_runtime(runtime: tokio::runtime::Runtime) {
    let started = std::time::Instant::now();
    let report = mj_core::runtime::shutdown(
        runtime,
        mj_core::runtime::BlockingWork::AwaitFor(DAEMON_BLOCKING_GRACE),
        DAEMON_BLOCKING_GRACE,
    );
    if report != mj_core::runtime::ShutdownReport::default() {
        // Subprocesses and SSH admissions register their waits by name; other
        // blocking work has no name, so the counts stand for it.
        let operations = mj_core::targets::active_blocking_operations()
            .unwrap_or_default()
            .into_iter()
            .map(|operation| {
                format!(
                    "{} ({}, {} ms)",
                    operation.purpose, operation.program, operation.elapsed_ms
                )
            })
            .collect::<Vec<_>>();
        tracing::warn!(
            awaits_left = report.awaits_left,
            blocking_left = report.blocking_left,
            ?operations,
            "the daemon exited leaving blocking work that no owner waits for"
        );
    }
    let took = started.elapsed();
    if took >= std::time::Duration::from_millis(250) {
        tracing::info!(
            duration_ms = took.as_millis(),
            "daemon runtime shutdown finished"
        );
    }
}

fn shutdown_dashboard_runtime(runtime: tokio::runtime::Runtime) {
    mj_core::runtime::shutdown(
        runtime,
        mj_core::runtime::BlockingWork::Abandon,
        RUNTIME_SHUTDOWN_GRACE,
    );
}

async fn run_command(
    command: Option<Command>,
    requested_workspace: Option<String>,
) -> Result<DashboardExit> {
    match command {
        None => run_workspace_dashboard(requested_workspace.as_deref(), false, None).await,
        Some(Command::Go(args)) => {
            ensure!(
                requested_workspace.is_none(),
                "mj go selects its workspace from the folder; omit --workspace"
            );
            let setup = args.setup || args.global_default;
            let mode = tokio::task::spawn_blocking(move || go::prepare(args))
                .await
                .context("prepare fast start")??;
            run_workspace_dashboard(None, false, Some((mode, setup))).await
        }
        Some(Command::Workspaces(args)) => match args.command {
            None => run_workspace_dashboard(requested_workspace.as_deref(), true, None).await,
            Some(WorkspacesCommand::List(args)) => api_commands::workspaces_list(args)
                .await
                .map(|()| DashboardExit::Normal),
            Some(WorkspacesCommand::Create(args)) => api_commands::workspaces_create(args)
                .await
                .map(|()| DashboardExit::Normal),
        },
        Some(Command::App) => desktop::run_desktop_app()
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::DesktopBootstrap) => desktop::desktop_bootstrap()
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Daemon(args)) => daemon_command(args).await.map(|()| DashboardExit::Normal),
        Some(Command::DaemonRun) => daemon::run_daemon_process()
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Acp(args)) => {
            let Some(workspace) = args.workspace.or(requested_workspace) else {
                return Err(workspace_required("mj acp").await);
            };
            // A running daemon's list, or the store's when none runs, as for
            // `mj new`: an unknown name is refused before anything is served
            // and without starting a daemon (launch finding R6-2).
            refuse_unknown_workspace(&workspace).await?;
            let workspace = Some(workspace);
            acp::serve(args, workspace)
                .await
                .map(|()| DashboardExit::Normal)
        }
        Some(Command::Doctor(args)) => doctor(args).map(|()| DashboardExit::Normal),
        Some(Command::Setup(args)) => setup(args).map(|()| DashboardExit::Normal),
        Some(Command::Import(args)) => {
            let workspace_id =
                resolve_store_workspace(args.workspace().or(requested_workspace).as_deref())
                    .await?;
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
        Some(Command::Move(args)) => move_session(args).await.map(|()| DashboardExit::Normal),
        Some(Command::MoveSources {
            session,
            cleanup,
            yes,
        }) => {
            anyhow::ensure!(
                cleanup.is_none() || yes,
                "Inspect with mj move-sources --session {session}, then pass --cleanup OPERATION --yes to delete that source and its excluded files"
            );
            let mut daemon = daemon::connect_or_start().await?;
            let sources = daemon.move_sources(session, cleanup).await?;
            println!("{}", serde_json::to_string_pretty(&sources)?);
            Ok(DashboardExit::Normal)
        }
        Some(Command::Login(args)) => login(args).await.map(|()| DashboardExit::Normal),
        Some(Command::New(args)) => {
            let workspace = args.workspace.or(requested_workspace);
            api_commands::new_session(args, workspace)
                .await
                .map(|()| DashboardExit::Normal)
        }
        Some(Command::Prompt(args)) => api_commands::prompt(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Wait(args)) => api_commands::wait(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::PutFile(args)) => api_commands::put_file(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Elicitations(args)) => api_commands::elicitations(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Respond(args)) => api_commands::respond(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Events(args)) => {
            let workspace = args.workspace.or(requested_workspace);
            api_commands::events(args, workspace)
                .await
                .map(|()| DashboardExit::Normal)
        }
        Some(Command::Usage(args)) => api_commands::usage(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Transcript(args)) => api_commands::transcript(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Diff(args)) => api_commands::diff(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Export(args)) => api_commands::export(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Sessions(args)) => {
            let workspace = args.workspace.or(requested_workspace);
            api_commands::sessions(args, workspace)
                .await
                .map(|()| DashboardExit::Normal)
        }
        Some(Command::Suspend(args)) => api_commands::suspend(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Destroy(args)) => api_commands::destroy(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Resume(mut args)) => {
            args.workspace.name = args.workspace.or(requested_workspace);
            api_commands::resume(args)
                .await
                .map(|()| DashboardExit::Normal)
        }
        Some(Command::InterruptTurn(args)) => api_commands::interrupt_turn(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Review(args)) => api_commands::review(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::StopTask(args)) => api_commands::stop_task(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::Models(args)) => api_commands::models(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::SetConfig(args)) => api_commands::set_config(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::ApiInfo(args)) => api_commands::api_info(args)
            .await
            .map(|()| DashboardExit::Normal),
        Some(Command::GithubToken(args)) => api_commands::github_token(args)
            .await
            .map(|()| DashboardExit::Normal),
        // Answered in `main` before anything starts.
        Some(command @ (Command::Close(_) | Command::CancelTurn(_))) => {
            bail!("{}", replacement_notice(Some(&command)).unwrap_or_default())
        }
    }
}

/// Run the daemon-owned move operation from the one-shot CLI.
///
/// Preparation is deliberately a separate request: it lets unattended callers
/// prove that a queue choice is explicit and lets interactive callers show the
/// interruption warning before the source is stopped. Once admitted, a lost
/// CLI connection does not cancel the daemon operation; only an explicit
/// Ctrl-C does.
async fn move_session(args: MoveArgs) -> Result<()> {
    if !args.yes
        && !args.prepare
        && (!std::io::IsTerminal::is_terminal(&std::io::stdin())
            || !std::io::IsTerminal::is_terminal(&std::io::stdout()))
    {
        let error = anyhow::anyhow!("non-interactive moves require --yes");
        if args.json {
            print_move_json(&mj_core::state::MoveOutcome {
                operation_id: String::new(),
                session_id: args.session.clone(),
                profile_id: args.profile.clone().unwrap_or_default(),
                target_template_id: args.target.clone().unwrap_or_default(),
                outcome: "failed".to_owned(),
                error: Some(error.to_string()),
                recovery: None,
            })?;
        }
        return Err(error);
    }
    let mut daemon = match daemon::connect_or_start().await {
        Ok(daemon) => daemon,
        Err(error) => {
            if args.json {
                print_move_json(&mj_core::state::MoveOutcome {
                    operation_id: String::new(),
                    session_id: args.session.clone(),
                    profile_id: args.profile.clone().unwrap_or_default(),
                    target_template_id: args.target.clone().unwrap_or_default(),
                    outcome: "failed".to_owned(),
                    error: Some(format!("{error:#}")),
                    recovery: None,
                })?;
            }
            return Err(error).context("connect to move daemon");
        }
    };
    let selection = MoveSelection {
        subagents: None,
        workspace: mj_core::move_workspace::WorkspaceSelection {
            exclusions: args
                .exclusions
                .iter()
                .map(|entry| {
                    let (repository, path) = entry
                        .split_once(':')
                        .context("--exclude requires REPOSITORY:PATH")?;
                    let path = mj_core::move_workspace::WorkspacePath {
                        repository: repository.into(),
                        path: path.into(),
                    };
                    path.validate()?;
                    Ok(path)
                })
                .collect::<Result<Vec<_>>>()?,
            acknowledge_large_transfer: args.allow_large_transfer,
        },
        session_id: args.session.clone(),
        profile_id: args.profile.clone(),
        target_template_id: args.target.clone(),
        additional_mounts: None,
        resource_allocation: None,
        clear_resource_allocation: args.clear_resources,
    };
    let preparation = match daemon.prepare_move_session(selection).await {
        Ok(preparation) => preparation,
        Err(error) => {
            if args.json {
                print_move_json(&mj_core::state::MoveOutcome {
                    operation_id: String::new(),
                    session_id: args.session.clone(),
                    profile_id: args.profile.clone().unwrap_or_default(),
                    target_template_id: args.target.clone().unwrap_or_default(),
                    outcome: "failed".to_owned(),
                    error: Some(format!("{error:#}")),
                    recovery: None,
                })?;
            }
            return Err(with_clear_resources_hint(error)).context("prepare move");
        }
    };
    if args.prepare {
        println!("{}", serde_json::to_string_pretty(&preparation)?);
        return Ok(());
    }
    if !args.json
        && preparation.destination_checks == mj_core::state::DestinationChecks::AfterProvisioning
    {
        eprintln!("{}", mj_core::state::EC2_MOVE_PREPARATION_NOTICE);
    }
    if let Some(workspace) = &preparation.workspace {
        use mj_core::move_workspace::format_bytes;
        eprintln!(
            "Workspace transfer: {}",
            format_bytes(preparation.selection.workspace.included_bytes(workspace))
        );
        for root in &workspace.roots {
            eprintln!("  {}", root.label());
        }
        if let Err(error) = preparation.selection.workspace.validate(workspace) {
            if args.json {
                print_move_json(&move_error_outcome(&preparation, error.to_string()))?;
            }
            return Err(with_large_transfer_hint(error));
        }
    }
    let pending = preparation.queued_commands.len();
    let queue = match (args.queue, pending, args.yes) {
        (Some(queue), _, _) => Some(queue.into()),
        (None, 0, _) => None,
        (None, _, true) => {
            let error = anyhow::anyhow!(queued_commands_need_choice(pending));
            if args.json {
                print_move_json(&move_error_outcome(&preparation, error.to_string()))?;
            }
            return Err(error);
        }
        (None, _, false) => Some(prompt_queue_choice(pending).await?),
    };

    print_move_conversion(&preparation);
    if args.yes {
        // --yes acknowledges interruption, but intentionally does not choose
        // what happens to queued work; that choice was handled above.
    } else if !prompt_move_confirmation(&preparation, queue).await? {
        let error = anyhow::anyhow!("move cancelled before interruption");
        if args.json {
            print_move_json(&move_error_outcome(&preparation, error.to_string()))?;
        }
        return Err(error);
    }

    if !args.json {
        eprintln!(
            "Moving {} to {}/{} (operation {})…",
            preparation.selection.session_id,
            preparation
                .selection
                .profile_id
                .as_deref()
                .unwrap_or(&preparation.source_profile_id),
            preparation
                .selection
                .target_template_id
                .as_deref()
                .unwrap_or(&preparation.source_target_template_id),
            preparation.operation_id
        );
    }
    let request = MoveSessionRequest {
        preparation: preparation.clone(),
        queue,
        acknowledge_interruption: true,
    };
    let mut operation = Box::pin(daemon.move_session(request));
    let outcome = tokio::select! {
        result = &mut operation => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for Ctrl-C while moving session")?;
            // Dropping the request future only detaches this client. Ask the
            // daemon explicitly, then bound the wait so a wedged target does
            // not make Ctrl-C appear ineffective.
            drop(operation);
            let cancel = async {
                let mut client = daemon::connect_or_start().await?;
                client.cancel_lifecycle(args.session.clone()).await
            };
            match tokio::time::timeout(std::time::Duration::from_secs(10), cancel).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(error = format!("{error:#}"), "move cancellation request failed"),
                Err(_) => tracing::warn!("timed out requesting move cancellation"),
            }
            let error = anyhow::anyhow!("move cancellation requested for {}", args.session);
            let cancelled = mj_core::state::MoveOutcome {
                operation_id: preparation.operation_id.clone(),
                session_id: preparation.selection.session_id.clone(),
                profile_id: preparation.selection.profile_id.clone().unwrap_or_else(|| preparation.source_profile_id.clone()),
                target_template_id: preparation.selection.target_template_id.clone().unwrap_or_else(|| preparation.source_target_template_id.clone()),
                outcome: "cancelled".to_owned(),
                error: Some(error.to_string()),
                recovery: Some("Reconnect to observe the daemon-owned move operation.".to_owned()),
            };
            if args.json {
                print_move_json(&cancelled)?;
            } else {
                print_move_human(&cancelled);
            }
            return Err(error);
        }
    };
    if args.json {
        print_move_json(&outcome)?;
    } else {
        print_move_human(&outcome);
    }
    if matches!(outcome.outcome.as_str(), "failed" | "cancelled") {
        bail!(
            "move {}: {}",
            outcome.outcome,
            outcome.error.as_deref().unwrap_or("no further details")
        );
    }
    Ok(())
}

/// Names the flags that resolve a refusal only when the refusal is about
/// consent to a large transfer; other selection failures are not fixed by them.
fn with_large_transfer_hint(error: anyhow::Error) -> anyhow::Error {
    if error.is::<mj_core::move_workspace::LargeTransferConsentRequired>() {
        error.context("use --prepare to inspect files, --allow-large-transfer to include all eligible data, or --exclude REPOSITORY:PATH to leave selected files at the source")
    } else {
        error
    }
}

/// The daemon owns the bare-target refusal text; the flag that resolves it is
/// specific to this command line, so it is added here.
fn with_clear_resources_hint(error: anyhow::Error) -> anyhow::Error {
    if format!("{error:#}").contains(mj_core::state::BARE_TARGET_FIXED_RESOURCES) {
        error.context("add --clear-resources to remove the inherited container sizing when moving to a fixed host")
    } else {
        error
    }
}

fn queued_commands_need_choice(pending: usize) -> String {
    format!(
        "{pending} queued command{} {} an explicit --queue discard|start with --yes",
        if pending == 1 { "" } else { "s" },
        if pending == 1 { "requires" } else { "require" }
    )
}

async fn prompt_queue_choice(pending: usize) -> Result<ResumeQueueDisposition> {
    let answer = tokio::task::spawn_blocking(move || {
        eprint!(
            "{pending} queued command{} found. [D]iscard queued work (default) or [S]tart after move? ",
            if pending == 1 { "" } else { "s" }
        );
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok::<_, io::Error>(line)
    })
    .await
    .context("queue choice prompt task failed")??;
    Ok(if answer.trim().eq_ignore_ascii_case("s") {
        ResumeQueueDisposition::Start
    } else {
        ResumeQueueDisposition::Discard
    })
}

/// Say what moving a local checkout into an isolated workspace will do. This
/// prints for `--yes` too: the checkout stays behind and uncommitted work is
/// copied, which a person should be able to read in the transcript of the
/// command afterwards.
fn print_move_conversion(preparation: &mj_core::state::MovePreparation) {
    let Some(conversion) = preparation.conversion.as_deref() else {
        return;
    };
    eprintln!("{}", conversion.summary_line());
    for warning in conversion.warning_lines() {
        eprintln!("{warning}");
    }
}

async fn prompt_move_confirmation(
    preparation: &mj_core::state::MovePreparation,
    queue: Option<ResumeQueueDisposition>,
) -> Result<bool> {
    let active = preparation.active;
    let source_profile = preparation.source_profile_id.clone();
    let source_target = preparation.source_target_template_id.clone();
    let profile = preparation
        .selection
        .profile_id
        .clone()
        .unwrap_or_else(|| source_profile.clone());
    let target = preparation
        .selection
        .target_template_id
        .clone()
        .unwrap_or_else(|| source_target.clone());
    let cross_harness = preparation.cross_harness;
    let in_place = preparation.in_place;
    let clear_resource_allocation = preparation.selection.clear_resource_allocation;
    let queued_commands = preparation.queued_commands.clone();
    let session_id = preparation.selection.session_id.clone();
    tokio::task::spawn_blocking(move || {
        eprintln!(
            "Move session {} from {source_profile}/{source_target} to {profile}/{target}.",
            session_id
        );
        if cross_harness {
            eprintln!("This changes harnesses; the transcript handoff is text-only.");
        }
        if in_place {
            eprintln!(
                "Only the harness and profile are replaced; the environment and workspace are kept."
            );
        }
        if active {
            if in_place {
                eprintln!("Active work will be interrupted; the session keeps its environment.");
            } else {
                eprintln!("Active work will be interrupted and restored into a fresh environment.");
            }
        }
        if clear_resource_allocation {
            eprintln!(
                "Destination uses fixed/default resources; inherited sizing will be removed."
            );
        }
        if let Some(queue) = queue {
            if !queued_commands.is_empty() {
                eprintln!(
                    "Queued work ({} command{}):",
                    queued_commands.len(),
                    if queued_commands.len() == 1 { "" } else { "s" }
                );
                for (index, command) in queued_commands.iter().enumerate() {
                    let (kind, text) = match &command.kind {
                        mj_core::state::QueuedCommandKind::Prompt => (
                            "prompt",
                            mj_core::transcript::materialized_content_text(&command.content),
                        ),
                        mj_core::state::QueuedCommandKind::SetConfig { key, value } => {
                            ("config", mj_core::state::config_command_text(key, value))
                        }
                    };
                    let text = text.replace('\n', " ");
                    let text = if text.chars().count() > 180 {
                        format!("{}…", text.chars().take(179).collect::<String>())
                    } else {
                        text
                    };
                    eprintln!("  {}. {kind}: {text}", index + 1);
                }
            }
            eprintln!(
                "Queued work will {} after destination readiness.",
                match queue {
                    ResumeQueueDisposition::Discard => "be discarded",
                    ResumeQueueDisposition::Start => "start",
                }
            );
        }
        eprint!("Continue? [y/N] ");
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok::<_, io::Error>(line.trim().eq_ignore_ascii_case("y"))
    })
    .await
    .context("move confirmation prompt task failed")?
    .map_err(Into::into)
}

fn move_error_outcome(
    preparation: &mj_core::state::MovePreparation,
    error: String,
) -> mj_core::state::MoveOutcome {
    mj_core::state::MoveOutcome {
        operation_id: preparation.operation_id.clone(),
        session_id: preparation.selection.session_id.clone(),
        profile_id: preparation
            .selection
            .profile_id
            .clone()
            .unwrap_or_else(|| preparation.source_profile_id.clone()),
        target_template_id: preparation
            .selection
            .target_template_id
            .clone()
            .unwrap_or_else(|| preparation.source_target_template_id.clone()),
        outcome: "failed".to_owned(),
        error: Some(error),
        recovery: None,
    }
}

fn print_move_json(outcome: &mj_core::state::MoveOutcome) -> Result<()> {
    println!("{}", serde_json::to_string(outcome)?);
    Ok(())
}

fn print_move_human(outcome: &mj_core::state::MoveOutcome) {
    let destination = format!("{}/{}", outcome.profile_id, outcome.target_template_id);
    match outcome.outcome.as_str() {
        "completed" => println!(
            "Moved {} to {destination}; ready and idle. (operation {})",
            outcome.session_id, outcome.operation_id
        ),
        "unchanged" => println!(
            "{} is already on {destination}; unchanged. (operation {})",
            outcome.session_id, outcome.operation_id
        ),
        "interrupted" => println!(
            "Move for {} will continue after daemon upgrade. (operation {})",
            outcome.session_id, outcome.operation_id
        ),
        other => println!(
            "Move {} for {} (operation {}){}{}",
            other,
            outcome.session_id,
            outcome.operation_id,
            outcome
                .error
                .as_deref()
                .map_or(String::new(), |error| format!(": {error}")),
            outcome
                .recovery
                .as_deref()
                .map_or(String::new(), |recovery| format!(" Recovery: {recovery}")),
        ),
    }
}

async fn run_workspace_dashboard(
    requested_workspace: Option<&str>,
    open_workspace_manager: bool,
    mut go: Option<(mj_tui::GoMode, bool)>,
) -> Result<DashboardExit> {
    let resume = dashboard::UpgradeResume::load().await?;
    if resume.is_some() {
        go = None;
    }
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let client_id = if let Some(resume) = &resume {
        resume.client_id.clone()
    } else {
        format!(
            "tui-{}-{}",
            std::process::id(),
            mj_core::workspace::new_workspace_id()?
        )
    };
    let (go_mode, go_setup) = go.map_or((None, false), |(mode, setup)| (Some(mode), setup));
    let startup = tokio::spawn(start_dashboard(
        resume.as_ref().map(|resume| resume.workspace_id.clone()),
        requested_workspace
            .filter(|_| resume.is_none())
            .map(str::to_owned),
        go_mode,
        client_id.clone(),
        interactive,
    ));
    // The splash takes the terminal first and plays while startup runs. A
    // terminal resuming after an upgrade goes straight back to work.
    let (startup, screen) = if interactive && resume.is_none() && splash::wanted() {
        let mut screen = dashboard::DashboardScreen::enter()?;
        let outcome =
            splash::play_until_loaded(&mut screen, startup, |startup: &DashboardStartup| {
                startup
                    .loaded
                    .as_ref()
                    .map_or(ratatui::style::Color::Reset, |loaded| loaded.background())
            })
            .await?;
        match outcome {
            // The dashboard plays the rest of the splash while it starts.
            splash::SplashOutcome::Ready((startup, playback)) => {
                screen.splash = Some(playback);
                (startup, Some(screen))
            }
            // Dropping the screen first puts the error in the normal scrollback.
            splash::SplashOutcome::Failed(error) => {
                drop(screen);
                return Err(error);
            }
            splash::SplashOutcome::Cancelled => return Ok(DashboardExit::Interrupted),
        }
    } else {
        let startup = startup.await.context("dashboard startup task failed")??;
        (startup, None)
    };
    let DashboardStartup {
        mut daemon,
        go: go_mode,
        loaded,
    } = startup;
    let go = go_mode.map(|mode| (mode, go_setup));

    // Attach only once the dashboard is certain to run, so a startup that
    // fails or is cancelled never leaves an attachment behind.
    daemon.attach(client_id.clone(), std::process::id()).await?;
    let attachment_cancellation = tokio_util::sync::CancellationToken::new();
    let attachment = daemon::maintain_attachment(
        client_id.clone(),
        std::process::id(),
        attachment_cancellation.clone(),
    );
    let result = match loaded {
        Some(loaded) => {
            let screen = match screen {
                Some(screen) => Ok(screen),
                None => dashboard::DashboardScreen::enter(),
            };
            match screen {
                Ok(screen) => {
                    run_dashboard_for_workspace(
                        loaded,
                        screen,
                        open_workspace_manager,
                        go,
                        attachment.presence,
                        resume,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
        }
        None => {
            println!("Welcome to Mjolnir");
            println!("Run `mj doctor` for non-interactive validation.");
            Ok(DashboardExit::Normal)
        }
    };
    attachment_cancellation.cancel();
    if let Err(error) = attachment.task.await {
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
    result
}

/// What the dashboard needs before it can draw.
struct DashboardStartup {
    daemon: daemon::DaemonClient,
    go: Option<mj_tui::GoMode>,
    /// The store as the dashboard first shows it; absent without a terminal.
    loaded: Option<dashboard::LoadedDashboard>,
}

/// Connects to the daemon, chooses the workspace, and loads the store. It
/// runs as its own task so the splash can animate meanwhile.
async fn start_dashboard(
    resumed_workspace: Option<String>,
    requested_workspace: Option<String>,
    mut go: Option<mj_tui::GoMode>,
    client_id: String,
    interactive: bool,
) -> Result<DashboardStartup> {
    let mut daemon = daemon::connect_or_start().await?;
    let workspaces = daemon.list_workspaces().await?;
    let selected = if let Some(resumed) = resumed_workspace.filter(|resumed| {
        workspaces
            .iter()
            .any(|workspace| workspace.workspace.id == *resumed)
    }) {
        resumed
    } else if let Some(mode) = &mut go {
        go::resolve_workspace(&mut daemon, mode).await?
    } else if let Some(requested) = requested_workspace {
        workspaces
            .iter()
            .find(|candidate| {
                candidate.workspace.name.to_lowercase() == requested.trim().to_lowercase()
            })
            .map(|candidate| candidate.workspace.id.clone())
            .ok_or_else(|| {
                unknown_workspace(
                    &requested,
                    workspaces.iter().map(|candidate| &candidate.workspace),
                )
            })?
    } else if let Some(workspace) = workspaces.first() {
        // The database orders workspaces by most recent opening.
        workspace.workspace.id.clone()
    } else {
        daemon
            .create_workspace(suggested_workspace_name(&workspaces)?)
            .await?
            .id
    };
    daemon.touch_workspace(selected.clone()).await?;
    let loaded = if interactive {
        // Listed again: choosing may have created a workspace.
        let workspaces = daemon
            .list_workspaces()
            .await?
            .into_iter()
            .map(|listing| listing.workspace)
            .collect();
        Some(
            tokio::task::spawn_blocking(move || {
                dashboard::LoadedDashboard::load(&selected, &client_id, workspaces)
            })
            .await
            .context("load dashboard state task failed")??,
        )
    } else {
        None
    };
    Ok(DashboardStartup { daemon, go, loaded })
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
            .ok_or_else(|| {
                unknown_workspace(
                    requested,
                    workspaces.iter().map(|candidate| &candidate.workspace),
                )
            });
    }
    match workspaces.as_slice() {
        [workspace] => Ok(workspace.workspace.id.clone()),
        [] => bail!("no workspace exists; run `mj` to create one"),
        _ => bail!("several workspaces exist; pass `--workspace NAME`"),
    }
}

/// The refusal for a command that creates a session without `--workspace`.
///
/// There is no hidden workspace to fall back on: every session lives in one
/// the dashboard and the viewer list, so the command has to name it (launch
/// finding H-3). The refusal lists what exists and how to make one.
///
/// It never starts a daemon: starting one only to refuse cost `mj new` more
/// than a second (launch finding R2-14), and `mj acp` must not start one
/// before a client has asked it for a session. A running daemon supplies the
/// list; without one the list is read from the store, and a store that does
/// not exist yet has none. A list that cannot be read is left out rather than
/// hiding the reason.
pub(crate) async fn workspace_required(command: &str) -> anyhow::Error {
    let workspaces = listed_workspaces_without_starting().await;
    let workspaces = workspaces.map(|workspaces| {
        workspaces
            .into_iter()
            .map(|workspace| (workspace.name, workspace.session_count))
            .collect::<Vec<_>>()
    });
    anyhow::anyhow!(workspace_required_message(command, workspaces.as_deref()))
}

/// The workspaces as a running daemon lists them or, when no daemon runs, as
/// the store holds them. Never starts a daemon. `None` when no list could be
/// read.
async fn listed_workspaces_without_starting() -> Option<Vec<mj_core::workspace::WorkspaceRecord>> {
    match daemon::connect_existing().await {
        Ok(mut daemon) => daemon
            .list_workspaces()
            .await
            .map(|workspaces| {
                workspaces
                    .into_iter()
                    .map(|listing| listing.workspace)
                    .collect::<Vec<_>>()
            })
            .map_err(|error| {
                tracing::warn!(%error, "could not list workspaces for the refusal");
            })
            .ok(),
        Err(error) if daemon::daemon_not_running(&error).is_some() => {
            tokio::task::spawn_blocking(stored_workspaces)
                .await
                .ok()
                .flatten()
        }
        Err(error) => {
            tracing::warn!(%error, "could not reach the daemon to list workspaces");
            None
        }
    }
}

/// Refuse a `--workspace` name that no workspace carries, before anything
/// starts a daemon (launch findings R5-9 and R6-2). The list comes from a
/// running daemon or, when none runs, from the store, as in
/// [`workspace_required`]. A name that cannot be checked is let through; the
/// daemon refuses it later if it is unknown.
pub(crate) async fn refuse_unknown_workspace(name: &str) -> Result<()> {
    let Some(workspaces) = listed_workspaces_without_starting().await else {
        return Ok(());
    };
    let wanted = name.trim().to_lowercase();
    if workspaces
        .iter()
        .any(|workspace| workspace.name.to_lowercase() == wanted)
    {
        return Ok(());
    }
    Err(unknown_workspace(name, &workspaces))
}

/// The workspaces in the store, read without a daemon. A store that does not
/// exist yet has none; one that cannot be read answers `None`.
fn stored_workspaces() -> Option<Vec<mj_core::workspace::WorkspaceRecord>> {
    if !mj_controller::database::database_path().exists() {
        return Some(Vec::new());
    }
    mj_controller::database::list_workspaces()
        .map_err(|error| {
            tracing::warn!(%error, "could not read the workspaces from the store");
        })
        .ok()
}

fn workspace_required_message(command: &str, workspaces: Option<&[(String, u64)]>) -> String {
    let mut message =
        format!("{command} needs --workspace NAME: every session lives in a workspace");
    push_workspace_list(&mut message, workspaces);
    message
}

/// The refusal for a `--workspace` name no workspace carries: the daemon's
/// workspaces and how to make one, as [`workspace_required_message`] gives
/// them (launch finding R2-5).
pub(crate) fn unknown_workspace_message(
    name: &str,
    workspaces: Option<&[(String, u64)]>,
) -> String {
    let mut message = format!("unknown workspace {name:?}");
    push_workspace_list(&mut message, workspaces);
    message
}

/// The same refusal from a daemon's workspace listing.
pub(crate) fn unknown_workspace<'a>(
    name: &str,
    workspaces: impl IntoIterator<Item = &'a mj_core::workspace::WorkspaceRecord>,
) -> anyhow::Error {
    let listed = workspaces
        .into_iter()
        .map(|workspace| (workspace.name.clone(), workspace.session_count))
        .collect::<Vec<_>>();
    anyhow::anyhow!(unknown_workspace_message(name, Some(&listed)))
}

/// Append the listed workspaces with their session counts, or say there are
/// none, and end with how to make one. `None` means the list could not be
/// read, so only the hint is added.
fn push_workspace_list(message: &mut String, workspaces: Option<&[(String, u64)]>) {
    match workspaces {
        Some([]) => message.push_str("; this instance has none yet"),
        Some(workspaces) => {
            message.push_str(". Workspaces:");
            for (name, sessions) in workspaces {
                let noun = if *sessions == 1 {
                    "session"
                } else {
                    "sessions"
                };
                message.push_str(&format!("\n  {name}  ({sessions} {noun})"));
            }
        }
        None => {}
    }
    message.push_str("\nCreate one with `mj workspaces create NAME`.");
}

/// The name plain `mj` gives the workspace it creates in `directory`: the
/// folder's own name. `mj go` looks for a workspace by this name, so the two
/// commands share one workspace per folder.
fn workspace_name_for_directory(directory: &std::path::Path) -> String {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("workspace")
        .trim()
        .chars()
        .take(64)
        .collect::<String>()
}

fn suggested_workspace_name(workspaces: &[daemon::WorkspaceListing]) -> Result<String> {
    let base = workspace_name_for_directory(
        &std::env::current_dir().context("read current directory for workspace name")?,
    );
    // The store keeps the name `default` for sessions made before a
    // workspace was required, even while that workspace is not listed.
    let names = workspaces
        .iter()
        .map(|candidate| candidate.workspace.name.to_lowercase())
        .chain([mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned()])
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
            let mut daemon = match daemon::connect_management().await {
                Ok(daemon) => daemon,
                // No endpoint file is the ordinary stopped state.
                Err(error) => {
                    if let Some(stopped) = daemon::daemon_not_running(&error) {
                        println!(
                            "Mjolnir daemon is stopped (no {}).",
                            stopped.metadata_path.display()
                        );
                        return Ok(());
                    }
                    return Err(error.context("Mjolnir daemon is not answering"));
                }
            };
            let status = daemon.status().await?;
            println!(
                "Mjolnir daemon {} (version {}) started {}; {} attached client{}; web viewer {}",
                status.pid,
                mj_client::build_identity::BuildIdentity::parse(&status.build_version)
                    .map_or_else(|_| status.build_version.clone(), |build| build.describe()),
                status.started_at,
                status.attached_clients,
                if status.attached_clients == 1 {
                    ""
                } else {
                    "s"
                },
                status.phone_status
            );
            // Two builds can report the same version, so the version line
            // cannot answer "is my rebuilt code running?". The executable file
            // can, and it is the question a development restart turns on.
            match daemon::process_runs_this_executable(status.pid)? {
                Some(true) => println!("This daemon runs this build."),
                Some(false) => println!(
                    "This daemon runs a different executable ({}) than this client ({}); \
                     commands work, but code you rebuilt is not running. \
                     Run `mj daemon restart` from this build.",
                    daemon::describe_executable(
                        daemon::process_executable_path(status.pid).as_deref()
                    ),
                    daemon::describe_executable(daemon::running_executable_path().as_deref()),
                ),
                None => {}
            }
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
            mj_core::config::ensure_may_control_store(
                &mj_core::config::data_dir(),
                "stop the Mjolnir daemon",
            )?;
            let daemon = match daemon::connect_management().await {
                Ok(daemon) => daemon,
                Err(error) if daemon::daemon_not_running(&error).is_some() => {
                    println!("Mjolnir daemon is already stopped.");
                    return Ok(());
                }
                Err(error) => return Err(error.context("Mjolnir daemon is not answering")),
            };
            daemon.stop_and_wait().await?;
            println!("Mjolnir daemon stopped; detached workers remain active.");
        }
        DaemonCommand::Restart => {
            let restarted = daemon::restart_daemon().await?;
            // A restart onto another build is an error, so reaching here means
            // the daemon is either proved to be this build or unidentifiable;
            // say which, because "restarted" alone is what used to mislead.
            let checked = match restarted.runs_this_build {
                Some(true) => ", running this build",
                _ => "",
            };
            println!(
                "Mjolnir daemon restarted as PID {}{checked}.",
                restarted.pid
            );
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
    if args.setup_token {
        return store_claude_setup_token(&profile_id, profile).await;
    }
    let (program, arguments) = mj_core::credentials::login_command(profile)
        .with_context(|| format!("profile {profile_id:?}"))?;
    let marker = profile.authentication_marker();
    let (before, _) = mj_core::credentials::read_credential_file(profile.kind, &marker)?;

    println!(
        "Running `{program} {}` against {}.",
        arguments.join(" "),
        profile.home.display()
    );
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        mj_core::config::HarnessHost::current(),
        &mut environment,
    );
    let status = tokio::process::Command::new(&program)
        .args(&arguments)
        .envs(&environment)
        .status()
        .await
        .map_err(|error| login_spawn_error(error, &program, profile.kind, &profile_id))
        .with_context(|| {
            format!(
                "run `{program} {}` for profile {profile_id}",
                arguments.join(" ")
            )
        })?;

    let (after, _) = mj_core::credentials::read_credential_file(profile.kind, &marker)?;
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

/// Say which program is missing and how to get it, rather than the bare
/// "No such file or directory" the operating system reports.
fn login_spawn_error(
    error: io::Error,
    program: &str,
    kind: mj_core::config::HarnessKind,
    profile_id: &str,
) -> anyhow::Error {
    if error.kind() != io::ErrorKind::NotFound {
        return error.into();
    }
    anyhow::anyhow!(
        "`{program}` is not installed or is not on PATH, so the {} login cannot run. {}, then run `mj login --profile {profile_id}` again.",
        kind.display_name(),
        kind.install_advice()
    )
}

/// Mint a long-lived Claude subscription token and store it for the profile.
///
/// Claude Code has no early-refresh command, and its `/login` grant rotates: a
/// container copy that reaches expiry at the same instant as the host spends a
/// single-use refresh token the host has already spent, and the container's
/// turn dies. `claude setup-token` mints a one-year token that never rotates,
/// and Claude Code reads it from `CLAUDE_CODE_OAUTH_TOKEN` ahead of the
/// credentials file, so there is nothing left to race.
async fn store_claude_setup_token(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> Result<()> {
    use mj_core::config::HarnessKind;
    use mj_core::credentials::CLAUDE_OAUTH_TOKEN_ENV;

    if profile.kind != HarnessKind::Claude {
        bail!(
            "--setup-token applies to Claude profiles only; profile {profile_id} is a {} profile",
            profile.kind.display_name()
        );
    }
    let token_path = mj_core::credentials::claude_oauth_token_path(profile_id);

    println!(
        "Running `claude setup-token` against {}.",
        profile.home.display()
    );
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        mj_core::config::HarnessHost::current(),
        &mut environment,
    );
    let output = tokio::task::spawn_blocking({
        let environment = environment.clone();
        move || {
            let mut command = std::process::Command::new("claude");
            command.arg("setup-token").envs(&environment);
            mj_core::subprocess::run_capturing_stdout(&mut command)
        }
    })
    .await
    .context("run `claude setup-token`")??;
    if !output.status.success() {
        bail!(
            "`claude setup-token` exited with {} for profile {profile_id}",
            output.status
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .context("`claude setup-token` printed non-UTF-8 output")?;
    let token = extract_setup_token(&stdout).with_context(|| {
        format!("`claude setup-token` printed no token for profile {profile_id}")
    })?;
    mj_core::credentials::write_claude_oauth_token(&token_path, token.as_bytes())?;

    let verify_token = token.clone();
    let verified = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new("claude");
        command
            .args(["auth", "status"])
            .envs(&environment)
            .env(CLAUDE_OAUTH_TOKEN_ENV, &verify_token);
        mj_core::subprocess::run_capturing_stdout(&mut command)
    })
    .await
    .context("run `claude auth status`")??;
    if !verified.status.success() {
        bail!(
            "the stored token did not authenticate: `claude auth status` exited with {}. \
             The token is at {}; remove it to go back to the synced credentials file.",
            verified.status,
            token_path.display()
        );
    }
    if let Some(method) = reported_auth_method(&verified.stdout)
        && method != "oauth_token"
    {
        bail!(
            "Claude Code authenticated with {method:?} rather than the stored token. \
             The token is at {}; remove it to go back to the synced credentials file.",
            token_path.display()
        );
    }

    println!(
        "Stored a long-lived Claude token for profile {profile_id} at {}.",
        token_path.display()
    );
    println!(
        "New and resumed sessions of this profile run with {CLAUDE_OAUTH_TOKEN_ENV} set, so they no longer race the host over a rotating login."
    );
    Ok(())
}

/// How `claude auth status` says it authenticated, when it says so.
///
/// Exit code alone proves little: `claude auth status` reports success for any
/// value of `CLAUDE_CODE_OAUTH_TOKEN`. What is worth confirming is that Claude
/// Code read the variable and gave it precedence, which the reported method
/// says. A build whose output carries no method is not a failure to report.
fn reported_auth_method(stdout: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()?
        .get("authMethod")?
        .as_str()
        .map(str::to_owned)
}

/// The token in `claude setup-token` output.
///
/// The command prints instructions as well as the token, so the last
/// whitespace-free `sk-ant-oat01-` line wins. A future format that stops using
/// that prefix still leaves the token as the last thing printed, so the last
/// non-empty line is the fallback.
fn extract_setup_token(stdout: &str) -> Option<String> {
    let candidates = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    candidates
        .iter()
        .rev()
        .find(|line| line.starts_with("sk-ant-oat01-") && !line.contains(char::is_whitespace))
        .or_else(|| candidates.last())
        .map(|line| line.to_string())
}

fn profile_ids(config: &Config) -> String {
    config
        .enabled_profiles()
        .map(|(id, _)| id.to_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn resolve_login_profile(config: &Config, requested: Option<&str>) -> Result<String> {
    if let Some(profile) = requested {
        let configured = config
            .profiles
            .get(profile)
            .with_context(|| format!("unknown profile {profile:?}"))?;
        ensure!(configured.enabled, "profile {profile:?} is disabled");
        return Ok(profile.to_owned());
    }
    let mut profiles = config.enabled_profiles().map(|(id, _)| id);
    match (profiles.next(), profiles.next()) {
        (Some(only), None) => Ok(only.to_owned()),
        (Some(_), Some(_)) => bail!(
            "several profiles are configured; pass --profile with one of: {}",
            profile_ids(config)
        ),
        (None, _) => bail!("no enabled agent profiles are configured; run `mj setup` first"),
    }
}

async fn recover(args: RecoverArgs) -> Result<()> {
    let mut daemon = daemon::connect_or_start().await?;
    match args.command {
        RecoverCommand::Scan {
            json,
            all_instances,
        } => {
            let scan = daemon.scan_recovery(all_instances).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&scan)?);
            } else {
                // An empty scan printed nothing, which reads the same as a
                // command that did not run (F-16).
                if scan.candidates.is_empty() {
                    match scan.hidden_other_instances {
                        0 => println!("nothing to recover"),
                        _ => println!("nothing to recover for this instance"),
                    }
                }
                for candidate in &scan.candidates {
                    let instance = match candidate.instance_id.as_deref() {
                        Some(instance) if instance == scan.instance_id => {
                            "this instance".to_owned()
                        }
                        Some(instance) => format!("instance {instance}"),
                        None => "unknown instance".to_owned(),
                    };
                    let metadata = if candidate.ownership.is_some() {
                        "ownership verified"
                    } else {
                        "v1 resource; profile and bundle unknown"
                    };
                    // A resource left behind by a session Hel still tracks
                    // cannot be adopted under that session id, so say which
                    // of the two recovery actions applies to it.
                    let metadata = match candidate.tracked_session {
                        Some(state) => format!(
                            "left by tracked session in state {}; destroy only",
                            state.as_str()
                        ),
                        None => metadata.to_owned(),
                    };
                    println!(
                        "{}\t{}\t{}\t{}",
                        candidate.session_id, candidate.target_template_id, instance, metadata
                    );
                }
                if scan.hidden_other_instances > 0 {
                    eprintln!(
                        "note: {} created by other or unknown instances were not listed; pass --all-instances to include them",
                        mj_core::text::counted(scan.hidden_other_instances, "worker", "workers")
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
            all_instances,
        } => {
            daemon
                .adopt_recovery(session.clone(), target, profile, bundle, all_instances)
                .await?;
            println!("adopted worker {session}");
            Ok(())
        }
        RecoverCommand::Destroy {
            session,
            target,
            confirm,
            all_instances,
        } => {
            daemon
                .destroy_recovery(session.clone(), target, confirm, all_instances)
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
                SetupPlatform::Linux => mj_controller::doctor::InstructionsPlatform::Linux,
                SetupPlatform::Macos => mj_controller::doctor::InstructionsPlatform::Macos,
            };
            print!("{}", mj_controller::doctor::setup_instructions(platform));
            Ok(())
        }
        None => run_setup_command(&config_path()),
    }
}

fn doctor(args: DoctorArgs) -> Result<()> {
    let checks = mj_controller::doctor::run_current(mj_controller::doctor::DoctorOptions {
        smoke: args.smoke,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        mj_controller::doctor::render_human(&checks, &mut io::stdout())?;
    }
    if mj_controller::doctor::all_ready(&checks) {
        Ok(())
    } else {
        Err(doctor_failure(args.json))
    }
}

fn doctor_failure(json: bool) -> anyhow::Error {
    if json {
        anyhow::anyhow!(
            "Mjolnir has fixable prerequisites; follow the `remediation` of each `fixable` check."
        )
    } else {
        anyhow::anyhow!(
            "Mjolnir has fixable prerequisites; follow the remediation lines under each `fixable` check above."
        )
    }
}

/// The prefix every message uses when it names a session, so notices stay
/// readable without losing which session they are about.
pub(crate) use mj_core::state::short_id;

pub(crate) struct TerminalGuard {
    pub(crate) terminal: Terminal<CrosstermBackend<io::Stdout>>,
    keyboard_enhancement: bool,
    /// The window title last written, so it is only rewritten when it
    /// changes and cleared on exit only if it was ever set.
    title: Option<String>,
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
            title: None,
        })
    }

    /// Rings the terminal bell. Terminals, multiplexers, and SSH clients
    /// pass BEL through, which is what makes it the one signal that works
    /// everywhere.
    pub(crate) fn ring_bell(&mut self) -> Result<()> {
        execute!(self.terminal.backend_mut(), crossterm::style::Print("\x07"))
            .context("ring the terminal bell")
    }

    /// Sets the terminal window title, writing only when it changed.
    pub(crate) fn set_title(&mut self, title: &str) -> Result<()> {
        if self.title.as_deref() == Some(title) {
            return Ok(());
        }
        execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::SetTitle(title)
        )
        .context("set the terminal title")?;
        self.title = Some(title.to_owned());
        Ok(())
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
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // A title this process set would otherwise outlive it in the tab.
        if self.title.is_some()
            && let Err(error) = execute!(
                self.terminal.backend_mut(),
                crossterm::terminal::SetTitle("")
            )
        {
            tracing::warn!(%error, "could not clear the terminal title");
        }
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
    use mj_core::state::{SessionRecord, SessionState, State};

    // Hard-won: 0475c51230: Launch finding C-2 found large-transfer consent flags incorrectly added to every move refusal; this checks they appear only for that consent error.
    #[test]
    fn move_error_hint_names_consent_flags_only_for_large_transfer_consent() {
        use mj_core::move_workspace::{WorkspaceAssessment, WorkspaceSelection};
        let blocked = WorkspaceAssessment {
            blockers: vec!["These local targets share the source worker storage.".into()],
            ..Default::default()
        };
        let error = WorkspaceSelection::default()
            .validate(&blocked)
            .unwrap_err();
        let shown = format!("{:#}", with_large_transfer_hint(error));
        assert!(!shown.contains("--allow-large-transfer"), "{shown}");
        assert!(shown.contains("share the source worker storage"), "{shown}");

        let large = WorkspaceAssessment {
            required_bytes: mj_core::move_workspace::LARGE_TRANSFER_BYTES,
            ..Default::default()
        };
        let error = WorkspaceSelection::default().validate(&large).unwrap_err();
        let shown = format!("{:#}", with_large_transfer_hint(error));
        assert!(shown.contains("--allow-large-transfer"), "{shown}");
    }

    // Hard-won: 0475c51230: Launch finding C-7 found the refusal say “1 queued command require”; this checks singular and plural counts agree.
    #[test]
    fn queued_command_refusal_agrees_in_number() {
        assert_eq!(
            queued_commands_need_choice(1),
            "1 queued command requires an explicit --queue discard|start with --yes"
        );
        assert_eq!(
            queued_commands_need_choice(2),
            "2 queued commands require an explicit --queue discard|start with --yes"
        );
    }

    /// F-16: `--session`, `--json`, and other flags had no description in
    /// `--help`. Every visible argument of every command now says what it is.
    // Hard-won: 820ba5c7eb: Finding F-16 found dozens of visible flags without help text; the recursive command-tree check rejects every undescribed argument.
    #[test]
    fn every_visible_argument_is_described_in_help() {
        fn undescribed(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in command.get_arguments() {
                if arg.is_hide_set() || ["help", "version"].contains(&arg.get_id().as_str()) {
                    continue;
                }
                if arg.get_help().is_none() && arg.get_long_help().is_none() {
                    missing.push(format!("{path} --{}", arg.get_id()));
                }
            }
            for subcommand in command.get_subcommands() {
                if subcommand.is_hide_set() {
                    continue;
                }
                undescribed(
                    subcommand,
                    &format!("{path} {}", subcommand.get_name()),
                    missing,
                );
            }
        }
        let mut missing = Vec::new();
        undescribed(
            &<Cli as clap::CommandFactory>::command(),
            "mj",
            &mut missing,
        );
        assert!(missing.is_empty(), "undescribed arguments: {missing:#?}");
    }

    /// R2-5: `mj new --workspace nosuch` said only `unknown workspace
    /// "nosuch"`. It now lists what exists and how to make one, in the shape
    /// the missing-flag refusal uses.
    // Hard-won: f0e899a4bf: Launch finding R2-5 found unknown workspace errors omitted available workspaces and creation guidance; the test checks the repaired refusal text.
    #[test]
    fn an_unknown_workspace_is_refused_with_the_workspaces_and_how_to_make_one() {
        let listed = [("beta".to_owned(), 0), ("alpha".to_owned(), 1)];
        let message = unknown_workspace_message("nosuch", Some(&listed));
        assert_eq!(
            message,
            "unknown workspace \"nosuch\". Workspaces:\n  beta  (0 sessions)\n  alpha  (1 session)\n\
             Create one with `mj workspaces create NAME`."
        );

        let message = unknown_workspace_message("nosuch", Some(&[]));
        assert_eq!(
            message,
            "unknown workspace \"nosuch\"; this instance has none yet\n\
             Create one with `mj workspaces create NAME`."
        );
    }

    /// F-16: `--workspace` was global, so every command's help offered it.
    // Hard-won: 820ba5c7eb: Finding F-16 found `--workspace` exposed globally on commands that ignore it; this checks acceptance only on commands that use it.
    #[test]
    fn workspace_is_offered_only_where_it_selects_something() {
        for argv in [
            vec!["mj", "new", "--workspace", "w", "--project-directory", "/p"],
            vec!["mj", "sessions", "--workspace", "w"],
            vec!["mj", "events", "--workspace", "w"],
            vec!["mj", "resume", "--wiki", "x", "--workspace", "w"],
            vec!["mj", "import", "codex", "--latest", "--workspace", "w"],
            vec!["mj", "acp", "--workspace", "w"],
            vec!["mj", "--workspace", "w", "new", "--project-directory", "/p"],
            vec!["mj", "--workspace", "w"],
        ] {
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
        }
        for argv in [
            vec!["mj", "prompt", "--session", "s", "hi", "--workspace", "w"],
            vec!["mj", "wait", "--session", "s", "--workspace", "w"],
            vec!["mj", "doctor", "--workspace", "w"],
        ] {
            assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?}");
        }
        let cli = Cli::try_parse_from(["mj", "sessions", "--workspace", "w"]).unwrap();
        let Some(Command::Sessions(args)) = cli.command else {
            panic!("expected the sessions command");
        };
        assert_eq!(args.workspace.or(None).as_deref(), Some("w"));
    }

    /// R2-10: the help said commands other than `mj new` and `mj acp` "need
    /// it when the instance has more than one", but with two workspaces
    /// `mj sessions` without it lists every session.
    // Hard-won: 12dc56b2e8: Launch finding R2-10 found help falsely saying `mj sessions` needs a workspace; the test checks required-command and filter descriptions.
    #[test]
    fn workspace_help_says_which_commands_require_it_and_which_filter() {
        let command = <Cli as clap::CommandFactory>::command();
        let argument = command
            .find_subcommand("sessions")
            .expect("the sessions command")
            .get_arguments()
            .find(|argument| argument.get_id() == "workspace_name")
            .expect("the --workspace option");
        let help = argument
            .get_long_help()
            .or(argument.get_help())
            .expect("the --workspace help")
            .to_string();
        assert!(!help.contains("other commands need it"), "{help}");
        assert!(help.contains("`mj new` and `mj acp` require it"), "{help}");
        assert!(
            help.contains("`mj sessions` and `mj events` show only that workspace"),
            "{help}"
        );
    }

    /// F-16: `mj acp` is documented, so `mj --help` lists it.
    // Hard-won: 820ba5c7eb: Finding F-16 found `mj acp` hidden from `mj --help`; this checks the public command list includes it.
    #[test]
    fn acp_is_listed_in_help() {
        let help = <Cli as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        assert!(help.contains("acp"), "{help}");
    }

    /// F-10: the removed names got clap's generic "unrecognized subcommand".
    // Hard-won: e5db6edb36: Finding F-10 found removed `close` and `cancel-turn` names yielded generic clap errors; the test checks replacement guidance for both spellings and keeps them hidden from help.
    #[test]
    fn a_removed_command_names_its_replacement_whatever_it_was_given() {
        for (argv, replacement) in [
            (
                vec!["mj", "close", "--session", "s1"],
                "`mj suspend` and `mj destroy`",
            ),
            (vec!["mj", "close"], "`mj suspend` and `mj destroy`"),
            (
                vec!["mj", "cancel-turn", "--session", "s1"],
                "`mj interrupt-turn`",
            ),
        ] {
            let cli = Cli::try_parse_from(&argv).expect("the old name still parses");
            let notice = replacement_notice(cli.command.as_ref()).expect("a notice");
            assert!(notice.contains(replacement), "{argv:?}: {notice}");
        }
        let help = <Cli as clap::CommandFactory>::command()
            .render_help()
            .to_string();
        assert!(
            !help.contains("cancel-turn"),
            "the old names stay out of --help"
        );
    }

    /// `mj login` for a harness whose CLI is not installed names the missing
    /// program and how to install it, instead of a bare ENOENT.
    // Hard-won: 613642d3f7: Launch finding J-12 found login exposed a bare ENOENT for missing Codex; this checks the program, installation command, and profile login remedy.
    #[test]
    fn a_missing_login_program_is_named_with_how_to_install_it() {
        let error = login_spawn_error(
            io::Error::from(io::ErrorKind::NotFound),
            "codex",
            mj_core::config::HarnessKind::Codex,
            "codex",
        );
        let message = format!("{error:#}");
        assert!(message.contains("`codex` is not installed"), "{message}");
        assert!(
            message.contains("npm install -g @openai/codex"),
            "{message}"
        );
        assert!(message.contains("mj login --profile codex"), "{message}");

        let other = login_spawn_error(
            io::Error::from(io::ErrorKind::PermissionDenied),
            "codex",
            mj_core::config::HarnessKind::Codex,
            "codex",
        );
        assert!(!format!("{other:#}").contains("not installed"));
    }

    // Hard-won: 594e6f27: Claude auth status accepted a made-up setup token by exit code alone.
    #[test]
    fn the_verification_reads_which_credential_claude_code_actually_used() {
        assert_eq!(
            reported_auth_method(br#"{"loggedIn":true,"authMethod":"oauth_token"}"#).as_deref(),
            Some("oauth_token")
        );
        assert_eq!(
            reported_auth_method(br#"{"loggedIn":true,"authMethod":"claudeai"}"#).as_deref(),
            Some("claudeai")
        );
        // Output that names no method leaves the exit code as the only check.
        assert_eq!(reported_auth_method(b"Logged in as someone\n"), None);
        assert_eq!(reported_auth_method(br#"{"loggedIn":true}"#), None);
    }

    /// The human report already prints every fix, so its closing line must
    /// point at them rather than send the user to `--json` for the same text.
    // Hard-won: 36382afd92: Launch finding J-5 found human doctor output printed fixes then pointed users to JSON; the test checks the closing guidance refers to the printed remediations.
    #[test]
    fn human_doctor_failure_points_at_the_printed_fixes() {
        let human = doctor_failure(false).to_string();
        assert!(!human.contains("--json"), "{human}");
        assert!(human.contains("remediation"), "{human}");
        assert!(doctor_failure(true).to_string().contains("remediation"));
    }

    #[test]
    fn failed_archive_removal_retains_session_metadata_for_retry() {
        let directory = tempfile::tempdir().unwrap();
        let archive_path = directory.path().join("checkpoint.hel.zip");
        std::fs::create_dir(&archive_path).unwrap();
        let session_id = "1123456789abcdef0123456789abcdef";
        let mut state = State::default();
        state.sessions.insert(
            session_id.into(),
            SessionRecord {
                project: None,
                target_runtime: None,
                launch_base: None,
                launch_branch: None,
                checkout: None,
                publication: None,
                build_cache: None,
                container_workspace: None,
                subagents: None,
                create_managed_worktree: None,
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                archived: false,
                container_cpus: None,
                container_memory: None,
                id: session_id.into(),
                title: "stopped".into(),
                harness_kind: mj_core::config::HarnessKind::Codex,
                last_profile: "codex".into(),
                bundle_id: "project".into(),
                project_directory: None,
                managed_worktree: None,
                review: None,
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
                checkpoint: Some(mj_core::state::CheckpointMetadata {
                    archive_path,
                    sha256: "a".repeat(64),
                    created_at: "2026-08-12T00:00:00Z".into(),
                    event_frontier: 7,
                }),
            },
        );
        let mut controller = Controller {
            config: Config::default(),
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

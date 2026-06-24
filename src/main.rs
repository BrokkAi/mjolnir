//! mjolnir: an interactive terminal client for any ACP-speaking agent.
//!
//! Starts sessions with the configured default ACP agent, falling back to the
//! picker only when no default exists or when the user explicitly requests a
//! new agent with `/new`. It persists global picker preferences to
//! `~/.config/mj/config.toml`, then spawns the agent as a child process and
//! renders the session in a ratatui chat UI.

mod acp;
mod app;
mod clipboard;
mod config;
mod event;
mod headless;
mod install;
mod notifications;
mod paths;
mod picker;
mod registry;
mod remote;
mod self_update;
mod session;
mod speech;
mod term;
mod ui;
mod version;
mod worktree;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::app::UiExitReason;
use crate::config::{
    Config, CustomAgent as ConfigCustomAgent, SelectedAgent, history_path, transcript_export_dir,
};
use crate::picker::{
    CustomAgent as PickerCustomAgent, PickerOutcome, PickerPreferences, PickerResult,
};
use crate::session::SessionEntryJson;
use crate::ui::{HeaderLabels, UiMode};
use crate::worktree::CreatedWorktree;

#[derive(Debug, Parser)]
#[command(name = "mj", version, about = "Interactive ACP chat TUI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Run one prompt non-interactively and print the result.
    ///
    /// Matches Claude Code's `--print`/`-p` shape where practical: provide
    /// the prompt as the optional value, or omit the value/read `-` to read
    /// stdin. Headless mode uses the configured agent from
    /// `~/.config/mj/config.toml`; it does not open the interactive picker.
    #[arg(short = 'p', long = "print", value_name = "PROMPT", num_args = 0..=1, default_missing_value = "-")]
    print: Option<String>,

    /// Output format for `--print`.
    #[arg(long, value_enum, default_value_t = HeadlessOutputFormat::Text)]
    output_format: HeadlessOutputFormat,

    /// Permission handling for `--print`.
    ///
    /// `default` rejects permission prompts so headless runs never hang.
    /// `acceptEdits` accepts edit/delete/move prompts but rejects shell
    /// execution. `bypassPermissions` accepts every permission prompt.
    #[arg(long, value_enum, default_value_t = HeadlessPermissionMode::Default)]
    permission_mode: HeadlessPermissionMode,

    /// Working directory used when opening a new session. Defaults to
    /// the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Use the legacy alternate-screen full-screen chat TUI.
    #[arg(long)]
    fullscreen_tui: bool,

    /// Resume an existing ACP session in headless mode instead of
    /// opening a new one.
    #[arg(long, hide = true)]
    resume_session: Option<String>,

    /// Path to a log file. When unset, logging is disabled because the
    /// TUI owns the terminal and stderr would corrupt the screen.
    #[arg(long = "debug-file", visible_alias = "log-file", env = "BROKK_TUI_LOG")]
    log_file: Option<PathBuf>,

    /// Run the ACP session in a Git worktree.
    ///
    /// With no value, creates a new linked worktree under
    /// <project>/.mjolnir/worktrees/ with a random adjective-noun name
    /// (e.g. `bold-robin`). With a value, reuses an existing worktree
    /// by name (short name under .mjolnir/worktrees/) or by path.
    #[arg(short = 'w', long, num_args = 0..=1, default_missing_value = "")]
    worktree: Option<String>,

    /// Capture the agent subprocess's stderr to this file. When unset
    /// the agent's stderr is discarded via `Stdio::null()` (/dev/null on
    /// Unix, NUL on Windows) so it doesn't scribble over the TUI.
    #[arg(long, env = "BROKK_TUI_AGENT_STDERR")]
    agent_stderr: Option<PathBuf>,

    /// Skip the startup check for a newer mj release.
    #[arg(long, global = true, env = "MJOLNIR_NO_UPDATE_CHECK")]
    no_update_check: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Resume an existing ACP session.
    ///
    /// Opens the agent picker first so the session is listed or loaded
    /// from the agent that owns it. Without a session ID, opens an
    /// interactive session picker for the chosen agent.
    ///
    /// Use `--list` to print sessions from the configured default agent
    /// in headless mode (no TUI).
    Resume(ResumeArgs),
    /// Start the local remote-control server.
    Server(ServerArgs),
}

#[derive(Debug, clap::Args, Default)]
struct ServerArgs {
    /// Public hostname to embed in the login QR code and TLS certificate.
    #[arg(long)]
    hostname: Option<String>,
    /// Days of disconnected-session history to keep. Sessions (and their
    /// queued prompts) whose last update is older are deleted by the
    /// periodic sweeper. Pass 0 to keep history forever.
    #[arg(long, default_value_t = 30)]
    history_days: u32,
}

#[derive(Debug, clap::Args)]
struct ResumeArgs {
    /// Session ID to resume from the chosen agent. When omitted, opens an
    /// interactive picker that fetches the chosen agent's session list.
    session_id: Option<String>,

    /// List available sessions and exit (headless, no TUI). Optionally
    /// filtered by `--cwd`.
    #[arg(short, long, conflicts_with = "session_id")]
    list: bool,

    /// Output format for `--list`.
    #[arg(long, value_enum, default_value_t = HeadlessOutputFormat::Text, requires = "list")]
    format: HeadlessOutputFormat,

    /// Working directory filter for `--list` and the resumed session.
    /// Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Run the resumed ACP session in a Git worktree.
    ///
    /// With no value, creates a new linked worktree under
    /// <project>/.mjolnir/worktrees/. With a value, reuses an existing
    /// worktree by name or by path.
    #[arg(short = 'w', long, num_args = 0..=1, default_missing_value = "")]
    worktree: Option<String>,

    /// Capture the agent subprocess's stderr to this file.
    #[arg(long, env = "BROKK_TUI_AGENT_STDERR")]
    agent_stderr: Option<PathBuf>,

    /// Use the legacy alternate-screen full-screen chat TUI.
    #[arg(long)]
    fullscreen_tui: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HeadlessOutputFormat {
    Text,
    Json,
    StreamJson,
}

impl From<HeadlessOutputFormat> for headless::OutputFormat {
    fn from(value: HeadlessOutputFormat) -> Self {
        match value {
            HeadlessOutputFormat::Text => Self::Text,
            HeadlessOutputFormat::Json => Self::Json,
            HeadlessOutputFormat::StreamJson => Self::StreamJson,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HeadlessPermissionMode {
    Default,
    #[value(name = "acceptEdits", alias = "accept-edits")]
    AcceptEdits,
    #[value(name = "bypassPermissions", alias = "bypass-permissions")]
    BypassPermissions,
}

impl From<HeadlessPermissionMode> for headless::PermissionMode {
    fn from(value: HeadlessPermissionMode) -> Self {
        match value {
            HeadlessPermissionMode::Default => Self::Default,
            HeadlessPermissionMode::AcceptEdits => Self::AcceptEdits,
            HeadlessPermissionMode::BypassPermissions => Self::BypassPermissions,
        }
    }
}

fn ui_mode(fullscreen_tui: bool) -> UiMode {
    if fullscreen_tui {
        UiMode::FullscreenTui
    } else {
        UiMode::InlineChat
    }
}

fn should_run_startup_update_check(cli: &Cli) -> bool {
    if cli.no_update_check || cli.print.is_some() {
        return false;
    }
    match &cli.command {
        Some(Commands::Resume(args)) => !args.list,
        Some(Commands::Server(_)) => false,
        None => true,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_file.as_deref())?;
    let fullscreen_tui = cli.fullscreen_tui;

    if should_run_startup_update_check(&cli)
        && let Err(e) = self_update::check_prompt_and_restart_if_accepted().await
    {
        tracing::warn!("startup update check failed: {e:#}");
    }

    let cwd = match cli.cwd.clone() {
        Some(p) => absolutize_cwd(p)?,
        None => std::env::current_dir().context("current dir")?,
    };

    // Dispatch to subcommand if provided.
    if let Some(command) = cli.command {
        return match command {
            Commands::Resume(mut args) => {
                args.fullscreen_tui |= fullscreen_tui;
                run_resume(args).await
            }
            Commands::Server(args) => {
                remote::run_server(args.hostname, args.history_days, cwd).await
            }
        };
    }

    if let Some(prompt_arg) = cli.print {
        let prompt = read_headless_prompt(prompt_arg)?;
        return headless::run(headless::RunConfig {
            prompt,
            cwd,
            resume_session: cli.resume_session,
            agent_stderr: cli.agent_stderr,
            output_format: cli.output_format.into(),
            permission_mode: cli.permission_mode.into(),
        })
        .await;
    }

    let (cwd, worktree) = prepare_worktree_for_arg(cwd, cli.worktree.as_deref())?;
    let worktree_label = worktree_label(worktree.as_ref());
    let project_label = project_label(&cwd);

    let result = run_app(
        cwd,
        cli.agent_stderr,
        project_label,
        worktree_label.clone(),
        None,
        None,
        ui_mode(fullscreen_tui),
    )
    .await;

    let worktree_kept = handle_worktree_after_tui(worktree.as_ref(), Some(ui_mode(fullscreen_tui)));

    // Print resume hint so the user can come back to this session.
    match &result {
        Ok(Some(session_id)) => {
            if worktree_kept {
                print_resume_hint(session_id, worktree_label.as_deref());
            }
        }
        Ok(None) => {}
        Err(_) => {}
    }

    result.map(|_| ())
}

/// Print a hint showing how to resume the session.
fn print_resume_hint(session_id: &str, worktree_label: Option<&str>) {
    if let Some(label) = worktree_label {
        println!("To resume: mj resume {session_id} --worktree {label}");
    } else {
        println!("To resume: mj resume {session_id}");
    }
}

/// Handle the `mj resume` subcommand: pick the agent to resume from, list
/// sessions, pick one interactively, or resume directly by ID.
async fn run_resume(args: ResumeArgs) -> Result<()> {
    let mode = ui_mode(args.fullscreen_tui);
    let cwd = match args.cwd.clone() {
        Some(p) => absolutize_cwd(p)?,
        None => std::env::current_dir().context("current dir")?,
    };
    let (cwd, worktree) = prepare_worktree_for_arg(cwd, args.worktree.as_deref())?;
    let worktree_label = worktree_label(worktree.as_ref());
    let project_label = project_label(&cwd);

    // `--list`: headless listing, print and exit.
    if args.list {
        // Load the configured agent (same as headless mode).
        let config_path = config::default_config_path();
        let cfg = Config::load(&config_path)
            .with_context(|| format!("load {}", config_path.display()))?;
        let agent = cfg.agent.ok_or_else(|| {
            anyhow::anyhow!(
                "no default agent configured; run `mj` once to pick an agent before listing sessions"
            )
        })?;
        let sessions = session::list_sessions(&agent, cwd, args.agent_stderr.as_deref()).await?;
        match args.format {
            HeadlessOutputFormat::Json | HeadlessOutputFormat::StreamJson => {
                let json: Vec<SessionEntryJson> =
                    sessions.iter().map(SessionEntryJson::from).collect();
                println!("{}", serde_json::to_string_pretty(&json)?);
            }
            HeadlessOutputFormat::Text => {
                if sessions.is_empty() {
                    println!("no sessions found");
                } else {
                    for s in &sessions {
                        let title = s.title.as_deref().unwrap_or("(untitled)");
                        let cwd_str = s.cwd.display();
                        let updated = s.updated_at.as_deref().unwrap_or("");
                        println!("{}  {}  {}  {}", s.session_id, title, cwd_str, updated);
                    }
                }
            }
        }
        if worktree.as_ref().is_some_and(|w| w.was_created) {
            let _ = handle_worktree_after_tui(worktree.as_ref(), None);
        }
        return Ok(());
    }

    let agent = pick_agent_for_resume().await;
    let Some(agent) = agent? else {
        eprintln!("Cancelled.");
        let _ = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
        return Ok(());
    };

    // Direct ID: launch the TUI with the chosen agent and session.
    if let Some(session_id) = args.session_id.clone() {
        // Look up the chosen session's title so the resumed header shows it
        // immediately rather than waiting for the agent's first
        // SessionInfoUpdate. A failed lookup is non-fatal — resume proceeds
        // with no title and the agent fills it in shortly after.
        let title =
            match session::list_sessions(&agent, cwd.clone(), args.agent_stderr.as_deref()).await {
                Ok(sessions) => sessions
                    .into_iter()
                    .find(|entry| entry.session_id == session_id)
                    .and_then(|entry| entry.title),
                Err(e) => {
                    tracing::warn!("list sessions for title lookup failed: {e:#}");
                    None
                }
            };
        let result = run_app(
            cwd,
            args.agent_stderr.clone(),
            project_label,
            worktree_label.clone(),
            Some(ResumeTarget {
                session_id: session_id.clone(),
                title,
            }),
            Some(agent),
            mode,
        )
        .await;
        let worktree_kept = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
        // Show resume hint for the session we just ran
        if let Ok(Some(resumed_id)) = &result
            && worktree_kept
        {
            print_resume_hint(resumed_id, worktree_label.as_deref());
        }
        return result.map(|_| ());
    }

    // Interactive picker: fetch sessions from the chosen agent first (agent is
    // killed after listing), then set up the TUI to show the session picker,
    // then launch the chosen session with a fresh process for the same agent.
    eprintln!("Fetching sessions from agent...");
    let sessions =
        session::list_sessions(&agent, cwd.clone(), args.agent_stderr.as_deref()).await?;
    if sessions.is_empty() {
        eprintln!("No sessions available.");
        let _ = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
        return Ok(());
    }

    let outcome = run_session_picker_once(sessions).await?;
    match outcome {
        session::ResumeOutcome::Cancelled => {
            eprintln!("Cancelled.");
            let _ = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
            Ok(())
        }
        session::ResumeOutcome::Selected(entry) => {
            eprintln!("Resuming session: {}", entry.session_id);
            let session_title = entry.title.clone();
            let result = run_app(
                cwd,
                args.agent_stderr,
                project_label,
                worktree_label.clone(),
                Some(ResumeTarget {
                    session_id: entry.session_id,
                    title: session_title,
                }),
                Some(agent),
                mode,
            )
            .await;
            let worktree_kept = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
            // Show resume hint for the session we just ran
            if let Ok(Some(resumed_id)) = &result
                && worktree_kept
            {
                print_resume_hint(resumed_id, worktree_label.as_deref());
            }
            result.map(|_| ())
        }
    }
}

async fn pick_agent_for_resume() -> Result<Option<SelectedAgent>> {
    let config_path = config::default_config_path();
    let mut cfg =
        Config::load(&config_path).with_context(|| format!("load {}", config_path.display()))?;

    let picker_result = run_agent_picker_once(&cfg).await?;
    apply_picker_preferences(&mut cfg, picker_result.preferences);
    let selected = picker_result.outcome.map(picker_outcome_to_selected);
    if cfg.agent.is_none()
        && let Some(agent) = selected.as_ref()
    {
        cfg.agent = Some(agent.clone());
    }
    cfg.save(&config_path)
        .with_context(|| format!("save {}", config_path.display()))?;
    Ok(selected)
}

fn read_headless_prompt(prompt_arg: String) -> Result<String> {
    if prompt_arg != "-" {
        return Ok(prompt_arg);
    }
    use std::io::Read;
    let mut prompt = String::new();
    std::io::stdin()
        .read_to_string(&mut prompt)
        .context("read prompt from stdin")?;
    Ok(prompt)
}

fn prepare_worktree_for_arg(
    cwd: PathBuf,
    worktree_arg: Option<&str>,
) -> Result<(PathBuf, Option<CreatedWorktree>)> {
    match worktree_arg {
        None => Ok((cwd, None)),
        Some("") => {
            // `--worktree` with no value: create a new one.
            let created = prepare_new_worktree(&cwd)?;
            Ok((created.session_cwd.clone(), Some(created)))
        }
        Some(name_or_path) => {
            // `--worktree <name>`: reuse an existing one.
            let opened = prepare_existing_worktree(&cwd, name_or_path)?;
            Ok((opened.session_cwd.clone(), Some(opened)))
        }
    }
}

fn absolutize_cwd(cwd: PathBuf) -> Result<PathBuf> {
    if cwd.is_absolute() {
        Ok(cwd)
    } else {
        Ok(std::env::current_dir().context("current dir")?.join(cwd))
    }
}

fn worktree_label(worktree: Option<&CreatedWorktree>) -> Option<String> {
    worktree.map(|w| paths::folder_label(&w.worktree_root))
}

fn project_label(cwd: &std::path::Path) -> String {
    paths::display_path_with_tilde(cwd)
}

fn handle_worktree_after_tui(worktree: Option<&CreatedWorktree>, mode: Option<UiMode>) -> bool {
    let Some(w) = worktree else {
        return true;
    };

    if mode == Some(UiMode::InlineChat) {
        // Inline mode restores the cursor to the host prompt row. Move to a
        // fresh line before printing post-session worktree messages so they do
        // not end up appended to the shell prompt.
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        if let Err(e) = writeln!(output) {
            tracing::warn!("worktree cleanup spacing failed: {e}");
        } else if let Err(e) = output.flush() {
            tracing::warn!("worktree cleanup spacing flush failed: {e}");
        }
    }

    // Remind the user where the worktree lives so they don't lose track
    // of their work — the alt-screen has just been torn down, so writes
    // to stdout now land in their normal scrollback.
    println!("Worktree: {}", w.worktree_root.display());
    if !w.was_created {
        return true;
    }

    // Offer to clean up a freshly-created worktree. Skip the prompt for
    // reused worktrees — the user explicitly asked to work in an
    // existing one, so removing it would be surprising.
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    match worktree::prompt_remove_on_exit(w, &mut input, &mut output) {
        Ok(removed) => !removed,
        Err(e) => {
            tracing::warn!("worktree cleanup prompt failed: {e:#}");
            true
        }
    }
}

fn prepare_new_worktree(cwd: &std::path::Path) -> Result<CreatedWorktree> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let created = worktree::create_for_cwd_prompting(cwd, &mut input, &mut output)?;
    tracing::info!(
        project_root = %created.project_root.display(),
        worktree_root = %created.worktree_root.display(),
        session_cwd = %created.session_cwd.display(),
        "created git worktree"
    );
    // Print before the TUI takes over the terminal so the path lands in
    // the user's normal scrollback and is visible during the session.
    println!("Created worktree: {}", created.worktree_root.display());
    Ok(created)
}

fn prepare_existing_worktree(cwd: &std::path::Path, name_or_path: &str) -> Result<CreatedWorktree> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let opened =
        worktree::open_existing_for_cwd_prompting(cwd, name_or_path, &mut input, &mut output)?;
    tracing::info!(
        project_root = %opened.project_root.display(),
        worktree_root = %opened.worktree_root.display(),
        session_cwd = %opened.session_cwd.display(),
        "reusing existing git worktree"
    );
    println!("Using worktree: {}", opened.worktree_root.display());
    Ok(opened)
}

fn should_open_initial_agent_picker(cfg: &Config, initial_agent: Option<&SelectedAgent>) -> bool {
    cfg.agent.is_none() && initial_agent.is_none()
}

async fn run_app(
    cwd: PathBuf,
    agent_stderr: Option<PathBuf>,
    project_label: String,
    worktree_label: Option<String>,
    resume_target: Option<ResumeTarget>,
    initial_agent: Option<SelectedAgent>,
    mode: UiMode,
) -> Result<Option<String>> {
    let config_path = config::default_config_path();
    let mut cfg = Config::load(&config_path)?;

    // Supervisor loop. Initial sessions use the configured default agent when
    // available. The picker is reserved for first-run setup and explicit
    // new-session requests (`/new` / Ctrl-N), while resumed sessions may provide
    // the agent chosen by `mj resume` or fall back to the configured default.
    // Consume resume_session and initial_agent on the first iteration only.
    let mut initial_resume = resume_target;
    let mut initial_agent = initial_agent;
    let mut pick_agent = should_open_initial_agent_picker(&cfg, initial_agent.as_ref());
    loop {
        let resume = initial_resume.take();
        let agent = if let Some(agent) = initial_agent.take() {
            agent
        } else if pick_agent {
            pick_agent = false;
            let picker_result = run_agent_picker_once(&cfg).await?;
            apply_picker_preferences(&mut cfg, picker_result.preferences);
            let Some(outcome) = picker_result.outcome else {
                cfg.save(&config_path)
                    .with_context(|| format!("save {}", config_path.display()))?;
                return Ok(None);
            };
            let selected = picker_outcome_to_selected(outcome);
            if cfg.agent.is_none() {
                cfg.agent = Some(selected.clone());
            }
            cfg.save(&config_path)
                .with_context(|| format!("save {}", config_path.display()))?;
            selected
        } else {
            cfg.agent.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "no default agent configured; run `mj` once to pick an agent before resuming sessions"
                )
            })?
        };

        let (reason, session_id, session_title) = run_session(
            &agent,
            cwd.clone(),
            agent_stderr.clone(),
            HeaderLabels {
                project: project_label.clone(),
                worktree: worktree_label.clone(),
                session_title: resume.as_ref().and_then(|target| target.title.clone()),
            },
            resume.as_ref().map(|target| target.session_id.clone()),
            mode,
        )
        .await?;
        match reason {
            UiExitReason::Quit => return Ok(session_id),
            UiExitReason::NewSession => {
                pick_agent = true;
                continue;
            }
            UiExitReason::ClearSession => {
                initial_agent = Some(agent);
                continue;
            }
            UiExitReason::LoadSession => {
                let sessions =
                    session::list_sessions(&agent, cwd.clone(), agent_stderr.as_deref()).await?;

                match session_picker_action(
                    run_session_picker_once(sessions).await?,
                    session_id,
                    session_title,
                ) {
                    SessionPickerAction::Resume { session_id, title } => {
                        initial_resume = Some(ResumeTarget { session_id, title });
                        initial_agent = Some(agent);
                        continue;
                    }
                    SessionPickerAction::Exit(session_id) => return Ok(session_id),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionPickerAction {
    Resume {
        session_id: String,
        title: Option<String>,
    },
    Exit(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeTarget {
    session_id: String,
    title: Option<String>,
}

fn session_picker_action(
    outcome: session::ResumeOutcome,
    current_session_id: Option<String>,
    current_session_title: Option<String>,
) -> SessionPickerAction {
    match outcome {
        session::ResumeOutcome::Selected(entry) => SessionPickerAction::Resume {
            session_id: entry.session_id,
            title: entry.title,
        },
        // Cancelling the picker keeps the current session running, so carry
        // its known title forward instead of dropping it — otherwise the
        // header title would blank out until the agent's next SessionInfoUpdate.
        session::ResumeOutcome::Cancelled => match current_session_id {
            Some(session_id) => SessionPickerAction::Resume {
                session_id,
                title: current_session_title,
            },
            None => SessionPickerAction::Exit(None),
        },
    }
}

async fn run_agent_picker_once(cfg: &Config) -> Result<PickerResult> {
    let mut terminal = ui::setup_fullscreen_terminal().context("setup terminal")?;
    let result = run_picker_with_registry(&mut terminal, cfg).await;
    if let Err(e) = ui::restore_fullscreen_terminal(&mut terminal) {
        tracing::warn!("restore terminal (agent picker) failed: {e}");
    }
    settle_after_fullscreen_picker_restore().await;
    result
}

async fn run_session_picker_once(
    sessions: Vec<session::SessionEntry>,
) -> Result<session::ResumeOutcome> {
    let mut terminal = ui::setup_fullscreen_terminal().context("setup terminal")?;
    let outcome = session::run_session_picker(&mut terminal, sessions).await;
    if let Err(e) = ui::restore_fullscreen_terminal(&mut terminal) {
        tracing::warn!("restore terminal (session picker) failed: {e}");
    }
    settle_after_fullscreen_picker_restore().await;
    outcome
}

async fn settle_after_fullscreen_picker_restore() {
    // Let the terminal finish leaving the alternate screen before the inline
    // viewport asks for a cursor position. Without this, some terminals answer
    // the CPR query late enough that crossterm times out and leaks the response
    // back to the shell prompt.
    tokio::time::sleep(Duration::from_millis(75)).await;
}

async fn run_picker_with_registry(
    terminal: &mut ratatui::Terminal<crate::term::TrackedBackend<std::io::Stdout>>,
    cfg: &Config,
) -> Result<PickerResult> {
    let cache_path = registry::default_cache_path();
    let registry =
        match registry::load_with_cache(&cache_path, registry::CACHE_TTL, registry::REGISTRY_URL)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "registry load failed, picker will offer anvil + custom only: {e:#}"
                );
                registry::Registry::default()
            }
        };
    picker::run_picker(
        terminal,
        &registry,
        &install::default_install_root(),
        &registry::current_platform(),
        picker_preferences_from_config(cfg),
    )
    .await
}

fn picker_preferences_from_config(cfg: &Config) -> PickerPreferences {
    PickerPreferences {
        default_agent: cfg.agent.as_ref().map(selected_to_picker_outcome),
        favorite_source_ids: cfg.favorite_agents.clone(),
        custom_agents: cfg
            .custom_agents
            .iter()
            .map(config_custom_to_picker_custom)
            .collect(),
    }
}

fn apply_picker_preferences(cfg: &mut Config, preferences: PickerPreferences) {
    cfg.agent = preferences.default_agent.map(picker_outcome_to_selected);
    cfg.favorite_agents = preferences.favorite_source_ids;
    cfg.custom_agents = preferences
        .custom_agents
        .into_iter()
        .map(picker_custom_to_config_custom)
        .collect();
}

fn config_custom_to_picker_custom(c: &ConfigCustomAgent) -> PickerCustomAgent {
    PickerCustomAgent {
        name: c.name.clone(),
        program: c.program.clone(),
        args: c.args.clone(),
        description: c.description.clone(),
    }
}

fn picker_custom_to_config_custom(c: PickerCustomAgent) -> ConfigCustomAgent {
    ConfigCustomAgent {
        name: c.name,
        program: c.program,
        args: c.args,
        description: c.description,
    }
}

fn picker_outcome_to_selected(o: PickerOutcome) -> SelectedAgent {
    SelectedAgent {
        source_id: o.source_id,
        program: o.program,
        args: o.args,
        env: o.env,
    }
}

fn selected_to_picker_outcome(agent: &SelectedAgent) -> PickerOutcome {
    PickerOutcome {
        source_id: agent.source_id.clone(),
        program: agent.program.clone(),
        args: agent.args.clone(),
        env: agent.env.clone(),
    }
}

fn agent_header_label(agent: &SelectedAgent) -> String {
    remote::agent_display_label(agent)
}

async fn run_session(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<PathBuf>,
    header_labels: HeaderLabels,
    resume_session: Option<String>,
    mode: UiMode,
) -> Result<(UiExitReason, Option<String>, Option<String>)> {
    let mut terminal = match mode {
        UiMode::InlineChat => {
            ui::setup_inline_chat_terminal(ui::INLINE_CHAT_HEIGHT).context("setup terminal")?
        }
        UiMode::FullscreenTui => ui::setup_fullscreen_terminal().context("setup terminal")?,
    };

    let (event_tx, runtime_event_rx) = mpsc::unbounded_channel();
    let (ui_event_tx, ui_event_rx) = mpsc::unbounded_channel();
    let (runtime_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel();

    let runtime_cfg = acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd,
        resume_session,
        env: agent.env.clone(),
        agent_stderr,
    };

    // Drive the ACP runtime on its own task so the UI can own the
    // current task's stdio (ratatui draws through stdout while ACP
    // talks to the agent's stdout/stdin, which are separate file
    // descriptors).
    let acp_handle = tokio::spawn(async move {
        if let Err(e) = acp::run(runtime_cfg, event_tx, cmd_rx).await {
            tracing::error!("acp runtime error: {e:#}");
        }
    });

    let hist_path = history_path();
    let export_dir = transcript_export_dir();
    // Pre-fill the UI header with the configured agent identity. Registry
    // agents use their source id so the header matches the picker/config,
    // while custom agents show the exact command line being launched.
    let agent_display_name = Some(agent_header_label(agent));
    let tracker_project_label = header_labels.project.clone();
    let remote_tracker = remote::RemoteSessionTracker::new(
        tracker_project_label,
        agent_header_label(agent),
        Some(runtime_cmd_tx.clone()),
        Some(ui_event_tx.clone()),
    );

    let event_tracker = remote_tracker.clone();
    let event_proxy = tokio::spawn(async move {
        let mut runtime_event_rx = runtime_event_rx;
        while let Some(event) = runtime_event_rx.recv().await {
            // Intercept before observing: permission prompts get their
            // responder wrapped so remote viewers see (and can answer)
            // the pending request.
            let event = event_tracker.intercept_event(event);
            event_tracker.observe_event(&event);
            if ui_event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let cmd_tracker = remote_tracker.clone();
    let cmd_proxy = tokio::spawn(async move {
        while let Some(command) = ui_cmd_rx.recv().await {
            cmd_tracker.observe_command(&command);
            if runtime_cmd_tx.send(command).is_err() {
                break;
            }
        }
    });

    let ui_result = ui::run(
        &mut terminal,
        cmd_tx,
        ui_event_rx,
        header_labels,
        agent_display_name,
        ui::UiPersistencePaths {
            history_path: Some(&hist_path),
            transcript_export_dir: export_dir.as_deref(),
        },
        mode,
    )
    .await;

    let restore_result = match mode {
        UiMode::InlineChat => ui::restore_inline_chat_terminal(&mut terminal),
        UiMode::FullscreenTui => ui::restore_fullscreen_terminal(&mut terminal),
    };
    if let Err(e) = restore_result {
        tracing::warn!("restore terminal failed: {e}");
    }
    if matches!(
        ui_result.as_ref().map(|(reason, _, _)| reason),
        Ok(UiExitReason::ClearSession)
    ) && let Err(e) = ui::clear_terminal_screen(&mut terminal)
    {
        tracing::warn!("clear terminal for /clear failed: {e}");
    }

    // Shutdown paths reaching this point:
    //
    // 1. User quit while idle (Ctrl-C/Ctrl-D/Esc with empty input):
    //    `ui::run` sends `UiCommand::Shutdown` and returns. `cmd_tx` is
    //    then dropped; `drive_session` sees `None` on its `recv()` and
    //    returns, then `acp::run` kills/reaps the child.
    //
    // 2. User cancelled mid-prompt and then quit: same as #1 once the
    //    cancel resolves into a `PromptDone(Cancelled)`. A force-quit
    //    via Ctrl-D before the cancel lands also works because
    //    `drive_prompt_turn` selects on the command channel and exits
    //    on `Shutdown` even while a prompt RPC is in flight.
    //
    // 3. Agent EOF / crash: `acp::run` races `drive_client` against
    //    `child.wait()`. The wait branch (or the post-drive snapshot)
    //    surfaces a single Fatal mentioning the unexpected exit, the
    //    UI flips to read-only, and the event channel closes.
    //
    // 4. Runtime wedged (e.g. agent stops responding but stdio stays
    //    open): the 2s `timeout` below trips and we `abort()` the
    //    task. `kill_on_drop(true)` on the `Command` then signals the
    //    child when the `Child` value is dropped during unwind.
    let abort_handle = acp_handle.abort_handle();
    match tokio::time::timeout(Duration::from_secs(2), acp_handle).await {
        Ok(join_res) => {
            if let Err(e) = join_res {
                tracing::warn!("acp task join: {e}");
            }
        }
        Err(_elapsed) => {
            tracing::warn!(
                "acp runtime did not exit within 2s; aborting (child may not be reaped)"
            );
            abort_handle.abort();
        }
    }

    wait_for_task("remote-control event proxy", event_proxy).await;
    wait_for_task("remote-control command proxy", cmd_proxy).await;
    remote_tracker.shutdown().await;

    ui_result
}

async fn wait_for_task(label: &str, handle: tokio::task::JoinHandle<()>) {
    let abort_handle = handle.abort_handle();
    match tokio::time::timeout(Duration::from_secs(2), handle).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!("{label} join failed: {error}");
        }
        Err(_) => {
            tracing::warn!("{label} did not exit within 2s; aborting");
            abort_handle.abort();
        }
    }
}

fn init_logging(path: Option<&std::path::Path>) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};

    let Some(path) = path else {
        return Ok(());
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).with_context(|| format!("create log dir {parent:?}"))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log {path:?}"))?;
    let filter =
        EnvFilter::try_from_env("BROKK_TUI_LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_writer(file)
        .with_env_filter(filter)
        .with_ansi(false)
        .init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::io::Write;

    #[test]
    fn agent_header_label_uses_registry_source_id() {
        let agent = SelectedAgent {
            source_id: "claude-acp".to_string(),
            program: PathBuf::from("npx"),
            args: vec!["-y".to_string(), "@x/claude@0.36.1".to_string()],
            env: Default::default(),
        };

        assert_eq!(agent_header_label(&agent), "claude-acp");
    }

    #[test]
    fn agent_header_label_uses_full_custom_command() {
        let agent = SelectedAgent {
            source_id: "custom".to_string(),
            program: PathBuf::from("/usr/local/bin/my agent"),
            args: vec!["--flag".to_string(), "value with space".to_string()],
            env: Default::default(),
        };

        assert_eq!(
            agent_header_label(&agent),
            "'/usr/local/bin/my agent' --flag 'value with space'"
        );
    }

    #[test]
    fn project_label_uses_full_worktree_session_path_with_tilde() {
        let worktree = CreatedWorktree {
            project_root: PathBuf::from("/Users/ryan/code/mjolnir"),
            worktree_root: PathBuf::from("/Users/ryan/code/mjolnir/.mjolnir/worktrees/bold-willow"),
            session_cwd: PathBuf::from(
                "/Users/ryan/code/mjolnir/.mjolnir/worktrees/bold-willow/src",
            ),
            was_created: false,
        };

        assert_eq!(
            project_label(&worktree.session_cwd),
            paths::display_path_with_tilde(&worktree.session_cwd)
        );
    }

    #[test]
    fn project_label_uses_full_directory_path_inside_mjolnir_worktree() {
        let cwd =
            std::path::Path::new("/Users/ryan/code/mjolnir/.mjolnir/worktrees/bold-willow/src");
        assert_eq!(project_label(cwd), paths::display_path_with_tilde(cwd));
    }

    #[test]
    fn project_label_uses_full_directory_path_without_worktree() {
        let cwd = std::path::Path::new("/Users/ryan/code/mjolnir/src");
        assert_eq!(project_label(cwd), paths::display_path_with_tilde(cwd));
    }

    #[test]
    fn inline_worktree_cleanup_output_starts_on_fresh_line() {
        let mut output = Vec::new();
        write!(&mut output, "shell$ ").expect("seed prompt");

        if Some(UiMode::InlineChat) == Some(UiMode::InlineChat) {
            writeln!(&mut output).expect("spacing newline");
            output.flush().expect("spacing flush");
        }
        writeln!(
            &mut output,
            "Worktree: /tmp/project/.mjolnir/worktrees/pale-tide"
        )
        .expect("worktree line");
        write!(&mut output, "Remove worktree 'pale-tide'? [y/N] ").expect("prompt");

        let rendered = String::from_utf8(output).expect("utf8");
        assert!(
            rendered.starts_with("shell$ \nWorktree: /tmp/project/.mjolnir/worktrees/pale-tide\n"),
            "inline cleanup output should begin on a fresh line: {rendered:?}"
        );
        assert!(
            rendered.contains("\nRemove worktree 'pale-tide'? [y/N] "),
            "cleanup prompt should not share the shell prompt line: {rendered:?}"
        );
    }

    #[test]
    fn initial_agent_picker_opens_only_without_default_or_initial_agent() {
        let configured = SelectedAgent {
            source_id: "claude-acp".to_string(),
            program: PathBuf::from("npx"),
            args: vec!["-y".to_string(), "@x/claude".to_string()],
            env: Default::default(),
        };
        let initial = SelectedAgent {
            source_id: "anvil".to_string(),
            program: PathBuf::from("uvx"),
            args: vec!["brokk".to_string(), "acp".to_string()],
            env: Default::default(),
        };

        assert!(should_open_initial_agent_picker(&Config::default(), None));
        assert!(!should_open_initial_agent_picker(
            &Config {
                agent: Some(configured),
                favorite_agents: Vec::new(),
                custom_agents: Vec::new(),
            },
            None
        ));
        assert!(!should_open_initial_agent_picker(
            &Config::default(),
            Some(&initial)
        ));

        // A named custom agent set as default skips the picker exactly like any
        // other configured default; the launch reads program/args from cfg.agent.
        let custom_default = SelectedAgent {
            source_id: "custom:my-agent".to_string(),
            program: PathBuf::from("/usr/local/bin/agent"),
            args: vec!["--flag".to_string()],
            env: Default::default(),
        };
        assert!(!should_open_initial_agent_picker(
            &Config {
                agent: Some(custom_default),
                favorite_agents: Vec::new(),
                custom_agents: Vec::new(),
            },
            None
        ));
    }

    #[test]
    fn picker_preferences_round_trip_custom_agents() {
        let cfg = Config {
            agent: None,
            favorite_agents: Vec::new(),
            custom_agents: vec![ConfigCustomAgent {
                name: "my-agent".to_string(),
                program: PathBuf::from("/usr/local/bin/agent"),
                args: vec!["--flag".to_string()],
                description: "test".to_string(),
            }],
        };
        let prefs = picker_preferences_from_config(&cfg);
        assert_eq!(prefs.custom_agents.len(), 1);
        assert_eq!(prefs.custom_agents[0].name, "my-agent");

        let mut roundtripped = Config::default();
        apply_picker_preferences(&mut roundtripped, prefs);
        assert_eq!(roundtripped.custom_agents.len(), 1);
        assert_eq!(roundtripped.custom_agents[0].name, "my-agent");
        assert_eq!(roundtripped.custom_agents[0].description, "test");
    }

    #[test]
    fn parse_accepts_debug_file_aliases() {
        let cli = Cli::try_parse_from(["mj", "--debug-file", "debug.log"]).expect("parse");
        assert_eq!(cli.log_file, Some(PathBuf::from("debug.log")));

        let cli = Cli::try_parse_from(["mj", "--log-file", "legacy.log"]).expect("parse");
        assert_eq!(cli.log_file, Some(PathBuf::from("legacy.log")));
    }

    #[test]
    fn parse_accepts_worktree_short_flag() {
        let cli = Cli::try_parse_from(["mj", "-w"]).expect("parse");
        assert_eq!(cli.worktree, Some(String::new()));

        let cli = Cli::try_parse_from(["mj", "-w", "named-tree"]).expect("parse");
        assert_eq!(cli.worktree.as_deref(), Some("named-tree"));
    }

    #[test]
    fn parse_accepts_fullscreen_tui_flags() {
        let cli = Cli::try_parse_from(["mj", "--fullscreen-tui"]).expect("parse");
        assert!(cli.fullscreen_tui);

        let cli = Cli::try_parse_from(["mj", "resume", "sess-123", "--fullscreen-tui"])
            .expect("parse resume");
        if let Some(Commands::Resume(args)) = cli.command {
            assert!(args.fullscreen_tui);
        } else {
            panic!("expected Resume subcommand");
        }

        let cli = Cli::try_parse_from(["mj", "--fullscreen-tui", "resume", "sess-123"])
            .expect("parse top-level resume");
        assert!(cli.fullscreen_tui);
    }

    #[test]
    fn startup_update_check_runs_only_for_interactive_modes() {
        let cli = Cli::try_parse_from(["mj"]).expect("parse");
        assert!(should_run_startup_update_check(&cli));

        let cli = Cli::try_parse_from(["mj", "--no-update-check"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = Cli::try_parse_from(["mj", "--print", "hi"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = Cli::try_parse_from(["mj", "resume", "--list"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = Cli::try_parse_from(["mj", "resume", "sess-123"]).expect("parse");
        assert!(should_run_startup_update_check(&cli));

        let cli = Cli::try_parse_from(["mj", "server"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));
    }

    #[test]
    fn parse_accepts_permission_mode_canonical_and_legacy_values() {
        let canonical = Cli::try_parse_from(["mj", "--permission-mode", "acceptEdits"])
            .expect("parse canonical");
        assert!(matches!(
            canonical.permission_mode,
            HeadlessPermissionMode::AcceptEdits
        ));

        let legacy =
            Cli::try_parse_from(["mj", "--permission-mode", "accept-edits"]).expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            HeadlessPermissionMode::AcceptEdits
        ));

        let canonical = Cli::try_parse_from(["mj", "--permission-mode", "bypassPermissions"])
            .expect("parse canonical");
        assert!(matches!(
            canonical.permission_mode,
            HeadlessPermissionMode::BypassPermissions
        ));

        let legacy = Cli::try_parse_from(["mj", "--permission-mode", "bypass-permissions"])
            .expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            HeadlessPermissionMode::BypassPermissions
        ));
    }

    #[test]
    fn parse_rejects_unknown_permission_mode_value() {
        let err = Cli::try_parse_from(["mj", "--permission-mode", "auto"]).expect_err("reject");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn parse_accepts_resume_session() {
        let cli = Cli::try_parse_from(["mj", "--print", "hi", "--resume-session", "sess-123"])
            .expect("parse");
        assert_eq!(cli.resume_session.as_deref(), Some("sess-123"));
    }

    #[test]
    fn help_shows_canonical_flags_and_values() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();

        assert!(help.contains("--debug-file <LOG_FILE>"));
        assert!(help.contains("[aliases: --log-file]"));
        assert!(help.contains("-w, --worktree [<WORKTREE>]"));
        assert!(help.contains("--fullscreen-tui"));
        assert!(!help.contains("--resume-session"));
        assert!(help.contains("[possible values: default, acceptEdits, bypassPermissions]"));
        assert!(!help.contains("accept-edits"));
        assert!(!help.contains("bypass-permissions"));
    }

    #[test]
    fn parse_resume_subcommand_without_args() {
        let cli = Cli::try_parse_from(["mj", "resume"]).expect("parse");
        assert!(matches!(cli.command, Some(Commands::Resume(_))));
        if let Some(Commands::Resume(args)) = cli.command {
            assert!(args.session_id.is_none());
            assert!(!args.list);
            assert!(matches!(args.format, HeadlessOutputFormat::Text));
            assert!(args.cwd.is_none());
            assert!(args.agent_stderr.is_none());
        }
    }

    #[test]
    fn parse_server_subcommand() {
        let cli = Cli::try_parse_from(["mj", "server"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => assert!(args.hostname.is_none()),
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_server_subcommand_with_global_cwd() {
        let cli = Cli::try_parse_from(["mj", "--cwd", "/tmp/test", "server"]).expect("parse");
        assert_eq!(cli.cwd, Some(PathBuf::from("/tmp/test")));
        assert!(matches!(cli.command, Some(Commands::Server(_))));
    }

    #[test]
    fn parse_server_subcommand_with_hostname() {
        let cli =
            Cli::try_parse_from(["mj", "server", "--hostname", "example.com"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert_eq!(args.hostname.as_deref(), Some("example.com"))
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_resume_subcommand_with_session_id() {
        let cli = Cli::try_parse_from(["mj", "resume", "sess-123"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.session_id, Some("sess-123".to_string()));
            assert!(!args.list);
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_with_list_flag() {
        let cli = Cli::try_parse_from(["mj", "resume", "--list"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert!(args.list);
            assert!(args.session_id.is_none());
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_with_list_and_format() {
        let cli =
            Cli::try_parse_from(["mj", "resume", "--list", "--format", "json"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert!(args.list);
            assert!(matches!(args.format, HeadlessOutputFormat::Json));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_with_cwd() {
        let cli = Cli::try_parse_from(["mj", "resume", "--cwd", "/tmp/test"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.cwd, Some(PathBuf::from("/tmp/test")));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_with_worktree() {
        let cli = Cli::try_parse_from(["mj", "resume", "sess-123", "--worktree", "named-tree"])
            .expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.session_id, Some("sess-123".to_string()));
            assert_eq!(args.worktree.as_deref(), Some("named-tree"));
        } else {
            panic!("expected Resume subcommand");
        }

        let cli = Cli::try_parse_from(["mj", "resume", "sess-123", "--worktree"])
            .expect("parse missing value");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.worktree.as_deref(), Some(""));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_rejects_list_with_session_id() {
        let err = Cli::try_parse_from(["mj", "resume", "sess-123", "--list"]).expect_err("reject");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parse_resume_subcommand_rejects_format_without_list() {
        let err = Cli::try_parse_from(["mj", "resume", "--format", "json"]).expect_err("reject");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parse_resume_subcommand_with_agent_stderr() {
        let cli =
            Cli::try_parse_from(["mj", "resume", "--agent-stderr", "agent.log"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.agent_stderr, Some(PathBuf::from("agent.log")));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_combined_flags() {
        let cli = Cli::try_parse_from([
            "mj",
            "resume",
            "sess-456",
            "--cwd",
            "/home/user",
            "--agent-stderr",
            "err.log",
        ])
        .expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.session_id, Some("sess-456".to_string()));
            assert_eq!(args.cwd, Some(PathBuf::from("/home/user")));
            assert_eq!(args.agent_stderr, Some(PathBuf::from("err.log")));
            assert!(!args.list);
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn cancelling_session_picker_resumes_current_session_preserving_title() {
        let action = session_picker_action(
            session::ResumeOutcome::Cancelled,
            Some("current-session".to_string()),
            Some("Current title".to_string()),
        );

        assert_eq!(
            action,
            SessionPickerAction::Resume {
                session_id: "current-session".to_string(),
                title: Some("Current title".to_string()),
            }
        );
    }

    #[test]
    fn cancelling_session_picker_without_current_session_exits() {
        let action = session_picker_action(session::ResumeOutcome::Cancelled, None, None);

        assert_eq!(action, SessionPickerAction::Exit(None));
    }

    #[test]
    fn selecting_session_picker_entry_resumes_selected_session() {
        let action = session_picker_action(
            session::ResumeOutcome::Selected(session::SessionEntry {
                session_id: "selected-session".into(),
                cwd: PathBuf::from("/tmp/project"),
                title: Some("My selected session".to_string()),
                updated_at: None,
            }),
            Some("current-session".to_string()),
            Some("ignored current title".to_string()),
        );

        assert_eq!(
            action,
            SessionPickerAction::Resume {
                session_id: "selected-session".to_string(),
                title: Some("My selected session".to_string()),
            }
        );
    }

    #[test]
    fn absolutize_cwd_resolves_relative_paths() {
        let cwd = absolutize_cwd(PathBuf::from("relative/project")).expect("absolutize");
        assert!(cwd.is_absolute());
        assert!(cwd.ends_with("relative/project"));

        let absolute = std::env::current_dir()
            .expect("current dir")
            .join("already");
        assert_eq!(
            absolutize_cwd(absolute.clone()).expect("absolute"),
            absolute
        );
    }

    #[test]
    fn resume_help_shows_subcommand_info() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert!(help.contains("resume"));
        assert!(help.contains("Resume an existing ACP session"));
    }
}

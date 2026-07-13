//! mjolnir: an interactive terminal client for any ACP-speaking agent.
//!
//! Resolves a model-first Thor/Loki/Eitri Council from DeepSWE and locally
//! launchable ACP adapters, then renders the active foreground ACP session in
//! a ratatui chat UI.

mod acp;
mod app;
mod archive;
mod claude_usage;
mod clipboard;
mod code_agent;
mod config;
mod council;
mod deepswe;
mod event;
mod headless;
mod labels;
mod loki;
mod menu;
mod model_resolve;
mod notifications;
mod palette;
mod paths;
mod probe;
mod qr;
mod ragnarok;
mod ragnarok_sprites;
mod remote;
mod self_update;
mod session;
mod session_provenance;
mod speech;
mod spinner;
mod tailscale;
mod term;
mod text;
mod theme;
mod ui;
mod version;
mod workspace_snapshot;
mod worktree;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::app::UiExitReason;
use crate::config::{Config, SelectedAgent, history_path, transcript_export_dir};
use crate::event::{
    InternalMessage, InternalMessageKind, LoadSessionResult, ReviewRole, UiCommand, UiEvent,
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

    /// Additional absolute workspace directory to expose to the agent.
    ///
    /// Repeat to pass multiple directories. These expand workspace scope
    /// for ACP file and terminal requests but do not imply trust.
    #[arg(
        long = "additional-directory",
        visible_alias = "add-dir",
        value_name = "PATH"
    )]
    additional_directories: Vec<PathBuf>,

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

    /// Maximum bytes for ACP filesystem text reads and writes.
    #[arg(
        long,
        global = true,
        env = "MJOLNIR_FS_MAX_TEXT_BYTES",
        default_value_t = acp::DEFAULT_FS_TEXT_BYTES,
        value_parser = parse_fs_max_text_bytes
    )]
    fs_max_text_bytes: u64,

    /// Skip the startup check for a newer mj release.
    #[arg(long, global = true, env = "MJOLNIR_NO_UPDATE_CHECK")]
    no_update_check: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Resume an existing ACP session.
    ///
    /// Uses saved provenance to route the session back to its original ACP
    /// adapter and model. Without an ID, opens an interactive session picker.
    ///
    /// Use `--list` to print sessions from the configured default agent
    /// in headless mode (no TUI).
    Resume(ResumeArgs),
    /// Start the local remote-control server.
    Server(ServerArgs),
}

fn parse_fs_max_text_bytes(value: &str) -> std::result::Result<u64, String> {
    let bytes = value
        .parse::<u64>()
        .map_err(|e| format!("invalid filesystem text byte limit: {e}"))?;
    if !(1..=acp::MAX_CONFIGURABLE_FS_TEXT_BYTES).contains(&bytes) {
        return Err(format!(
            "filesystem text byte limit must be between 1 and {}",
            acp::MAX_CONFIGURABLE_FS_TEXT_BYTES
        ));
    }
    Ok(bytes)
}

#[derive(Debug, clap::Args, Default)]
struct ServerArgs {
    /// Public hostname to embed in the login QR code and TLS certificate.
    #[arg(long)]
    hostname: Option<String>,
    /// Serve a trusted HTTPS certificate for this machine's tailscale
    /// (ts.net) name, minted via `tailscale cert`, so tailnet devices get no
    /// browser certificate warning. Requires tailscale to be running with
    /// MagicDNS and HTTPS Certificates enabled on the tailnet.
    #[arg(long, conflicts_with = "hostname")]
    tailscale: bool,
    /// Days of disconnected-session history to keep. Sessions (and their
    /// queued prompts) whose last update is older are deleted by the
    /// periodic sweeper. Pass 0 to keep history forever.
    #[arg(long, default_value_t = 30)]
    history_days: u32,
    /// Days a remote-viewer browser/PWA stays signed in before it must
    /// re-authenticate. Pass 0 for an ephemeral session that ends when the
    /// browser/PWA closes.
    #[arg(long, default_value_t = remote::DEFAULT_SESSION_TTL_DAYS)]
    session_ttl_days: u32,
    /// Sign every device out by rotating the cookie signing key on startup. The
    /// QR/bearer token is preserved, so devices can re-authenticate as usual.
    #[arg(long)]
    logout_all: bool,
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

    /// Additional absolute workspace directory to expose to the resumed agent.
    ///
    /// Repeat to pass multiple directories. These expand workspace scope
    /// for ACP file and terminal requests but do not imply trust.
    #[arg(
        long = "additional-directory",
        visible_alias = "add-dir",
        value_name = "PATH"
    )]
    additional_directories: Vec<PathBuf>,

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
    let fs_max_text_bytes = cli.fs_max_text_bytes;
    let top_level_additional_directories = cli.additional_directories.clone();

    if let Some(command) = cli.command {
        return match command {
            Commands::Resume(mut args) => {
                args.fullscreen_tui |= fullscreen_tui;
                run_resume(args, fs_max_text_bytes, top_level_additional_directories).await
            }
            Commands::Server(args) => {
                let workspace_roots =
                    validate_workspace_roots(&cwd, &top_level_additional_directories)?;
                remote::run_server(remote::ServerOptions {
                    hostname: args.hostname,
                    tailscale: args.tailscale,
                    history_days: args.history_days,
                    session_ttl_days: args.session_ttl_days,
                    logout_all: args.logout_all,
                    cwd,
                    additional_directories: workspace_roots.additional_directories().to_vec(),
                    fs_max_text_bytes,
                })
                .await
            }
        };
    }

    if let Some(prompt_arg) = cli.print {
        let workspace_roots = validate_workspace_roots(&cwd, &top_level_additional_directories)?;
        let prompt = read_headless_prompt(prompt_arg)?;
        return headless::run(headless::RunConfig {
            prompt,
            cwd,
            additional_directories: workspace_roots.additional_directories().to_vec(),
            resume_session: cli.resume_session,
            agent_stderr: cli.agent_stderr,
            fs_max_text_bytes,
            output_format: cli.output_format.into(),
            permission_mode: cli.permission_mode.into(),
        })
        .await;
    }

    let (cwd, worktree) = prepare_worktree_for_arg(cwd, cli.worktree.as_deref())?;
    let workspace_roots = validate_workspace_roots(&cwd, &top_level_additional_directories)?;
    let worktree_label = worktree_label(worktree.as_ref());
    let project_label = project_label(&cwd);
    let result = run_app(
        cwd,
        RuntimeOptions {
            agent_stderr: cli.agent_stderr,
            additional_directories: workspace_roots.additional_directories().to_vec(),
            fs_max_text_bytes,
        },
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
                print_resume_hint(
                    ui_mode(fullscreen_tui),
                    session_id,
                    worktree_label.as_deref(),
                    workspace_roots.additional_directories(),
                );
            }
        }
        Ok(None) => {}
        Err(_) => {}
    }

    result.map(|_| ())
}

/// Print a hint showing how to resume the session.
fn print_resume_hint(
    mode: UiMode,
    session_id: &str,
    worktree_label: Option<&str>,
    additional_roots: &[PathBuf],
) {
    println!(
        "{}",
        resume_hint_output(mode, session_id, worktree_label, additional_roots)
    );
}

/// Build the post-session resume hint text.
///
/// Inline mode leaves the cursor on the host shell's prompt row after teardown,
/// so a bare `println!` writes the hint onto that row where the shell overwrites
/// it when it repaints its prompt — the same collision `handle_worktree_after_tui`
/// avoids for worktree output. Leading with a newline moves off the prompt row
/// first. Fullscreen restores via the primary buffer, so its output already
/// lands on a fresh line and needs no lead.
fn resume_hint_output(
    mode: UiMode,
    session_id: &str,
    worktree_label: Option<&str>,
    additional_roots: &[PathBuf],
) -> String {
    let lead = if mode == UiMode::InlineChat { "\n" } else { "" };
    format!(
        "{lead}To resume: {}",
        resume_hint_command(session_id, worktree_label, additional_roots)
    )
}

fn resume_hint_command(
    session_id: &str,
    worktree_label: Option<&str>,
    additional_roots: &[PathBuf],
) -> String {
    let mut command = format!("mj resume {}", shell_quote(session_id));
    if let Some(label) = worktree_label {
        command.push_str(" --worktree ");
        command.push_str(&shell_quote(label));
    }
    for root in additional_roots {
        command.push_str(" --additional-directory ");
        command.push_str(&shell_quote(&root.display().to_string()));
    }
    command
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn primary_session_routes(council: &council::ResolvedCouncil) -> Vec<council::ResolvedRole> {
    let mut routes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for role in std::iter::once(&council.thor).chain(council.available.iter()) {
        if role.ranked && seen.insert(role.launch.source_id.clone()) {
            routes.push(role.clone());
        }
    }
    routes
}

async fn list_council_sessions(
    council: &council::ResolvedCouncil,
    cwd: &Path,
    agent_stderr: Option<&Path>,
) -> Vec<session::SessionEntry> {
    let mut sessions = Vec::new();
    for role in primary_session_routes(council) {
        let agent = selected_agent_for_role(&role);
        match session::list_sessions_with_capabilities(&agent, cwd.to_path_buf(), agent_stderr)
            .await
        {
            Ok(mut listing) => {
                for entry in &mut listing.sessions {
                    entry.adapter_source_id = Some(role.launch.source_id.clone());
                    if let Some(record) = session_provenance::find(&entry.session_id, &entry.cwd)
                        && record.adapter_source_id == role.launch.source_id
                    {
                        entry.model = Some(record.model);
                    } else {
                        entry.model = Some(role.model.model.clone());
                    }
                    entry.delete_supported = listing.delete_supported;
                }
                sessions.extend(listing.sessions);
            }
            Err(error) => tracing::warn!(
                adapter = %role.launch.source_id,
                "list Council sessions: {error:#}"
            ),
        }
    }
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| a.adapter_source_id.cmp(&b.adapter_source_id))
    });
    sessions
}

fn role_for_session_entry<'a>(
    council: &'a council::ResolvedCouncil,
    entry: &session::SessionEntry,
) -> Option<&'a council::ResolvedRole> {
    let adapter = entry.adapter_source_id.as_deref()?;
    entry
        .model
        .as_deref()
        .and_then(|model| {
            council
                .available
                .iter()
                .find(|role| role.launch.source_id == adapter && role.model.model == model)
        })
        .or_else(|| {
            council
                .available
                .iter()
                .find(|role| role.launch.source_id == adapter && role.ranked)
        })
}

/// Handle the `mj resume` subcommand: pick the agent to resume from, list
/// sessions, pick one interactively, or resume directly by ID.
async fn run_resume(
    args: ResumeArgs,
    fs_max_text_bytes: u64,
    top_level_additional_directories: Vec<PathBuf>,
) -> Result<()> {
    let mode = ui_mode(args.fullscreen_tui);
    let cwd = match args.cwd.clone() {
        Some(p) => absolutize_cwd(p)?,
        None => std::env::current_dir().context("current dir")?,
    };
    let mut requested_additional_directories = top_level_additional_directories;
    requested_additional_directories.extend(args.additional_directories.iter().cloned());
    let (cwd, worktree) = prepare_worktree_for_arg(cwd, args.worktree.as_deref())?;
    let workspace_roots = validate_workspace_roots(&cwd, &requested_additional_directories)?;
    let additional_directories = workspace_roots.additional_directories().to_vec();
    let worktree_label = worktree_label(worktree.as_ref());
    let project_label = project_label(&cwd);
    let cfg = Config::load(&config::default_config_path())?;
    let mut resume_council = council::resolve(&cfg, &cwd).await?;
    let mut agent = selected_agent_for_role(&resume_council.thor);
    if let Some(session_id) = args.session_id.as_deref()
        && let Some(record) = session_provenance::find(session_id, &cwd)
    {
        let pinned = resume_council
            .available
            .iter()
            .find(|role| {
                role.model.model == record.model
                    && role.model_value == record.model_value
                    && role.launch.source_id == record.adapter_source_id
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session {session_id} belongs to {} via {}, which is not currently launchable",
                    record.model,
                    record.adapter_source_id
                )
            })?;
        resume_council.thor = pinned.clone();
        agent = selected_agent_for_role(pinned);
    } else if let Some(session_id) = args.session_id.as_deref() {
        let matches = list_council_sessions(&resume_council, &cwd, args.agent_stderr.as_deref())
            .await
            .into_iter()
            .filter(|entry| entry.session_id == session_id)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [entry] => {
                let role = role_for_session_entry(&resume_council, entry)
                    .ok_or_else(|| anyhow::anyhow!("session {session_id} has no launchable route"))?
                    .clone();
                session_provenance::record(session_provenance::Record {
                    session_id: session_id.to_string(),
                    cwd: entry.cwd.clone(),
                    adapter_source_id: role.launch.source_id.clone(),
                    model: role.model.model.clone(),
                    model_value: role.model_value.clone(),
                });
                agent = selected_agent_for_role(&role);
                resume_council.thor = role;
            }
            [] => {}
            _ => anyhow::bail!(
                "legacy session ID {session_id} is ambiguous across Council adapters; select it with `mj resume` first"
            ),
        }
    }

    // `--list`: headless listing, print and exit.
    if args.list {
        let sessions =
            list_council_sessions(&resume_council, &cwd, args.agent_stderr.as_deref()).await;
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
            RuntimeOptions {
                agent_stderr: args.agent_stderr.clone(),
                additional_directories: additional_directories.clone(),
                fs_max_text_bytes,
            },
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
            print_resume_hint(
                mode,
                resumed_id,
                worktree_label.as_deref(),
                workspace_roots.additional_directories(),
            );
        }
        return result.map(|_| ());
    }

    let mut notice = None;
    loop {
        // Interactive picker: fetch sessions from the chosen agent first (agent is
        // killed after listing), then set up the TUI to show the session picker,
        // then launch the chosen session with a fresh process for the same agent.
        eprintln!("Fetching sessions from agent...");
        let sessions =
            list_council_sessions(&resume_council, &cwd, args.agent_stderr.as_deref()).await;
        if sessions.is_empty() {
            eprintln!("No sessions available.");
            let _ = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
            return Ok(());
        }

        let outcome = run_session_picker_once(
            sessions,
            true,
            notice.take(),
            Config::load(&config::default_config_path())
                .map(|cfg| cfg.theme.palette())
                .unwrap_or_else(|_| theme::TerminalThemeKind::default().palette()),
        )
        .await?;
        match outcome {
            session::ResumeOutcome::Cancelled => {
                eprintln!("Cancelled.");
                let _ = handle_worktree_after_tui(worktree.as_ref(), Some(mode));
                return Ok(());
            }
            session::ResumeOutcome::DeleteRequested(entry) => {
                notice = if entry.delete_supported {
                    match role_for_session_entry(&resume_council, &entry) {
                        Some(role) => {
                            let route = selected_agent_for_role(role);
                            Some(
                                delete_session_notice(&route, entry, args.agent_stderr.as_deref())
                                    .await,
                            )
                        }
                        None => Some("Delete failed: session route is unavailable".to_string()),
                    }
                } else {
                    Some("This ACP adapter does not support session deletion".to_string())
                };
            }
            session::ResumeOutcome::Selected(entry) => {
                eprintln!("Resuming session: {}", entry.session_id);
                let session_title = entry.title.clone();
                let role = role_for_session_entry(&resume_council, &entry)
                    .ok_or_else(|| anyhow::anyhow!("selected session route is unavailable"))?
                    .clone();
                agent = selected_agent_for_role(&role);
                resume_council.thor = role;
                let result = run_app(
                    cwd,
                    RuntimeOptions {
                        agent_stderr: args.agent_stderr,
                        additional_directories: additional_directories.clone(),
                        fs_max_text_bytes,
                    },
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
                    print_resume_hint(
                        mode,
                        resumed_id,
                        worktree_label.as_deref(),
                        workspace_roots.additional_directories(),
                    );
                }
                return result.map(|_| ());
            }
        }
    }
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

fn validate_workspace_roots(
    cwd: &Path,
    additional_directories: &[PathBuf],
) -> Result<paths::WorkspaceRoots> {
    paths::WorkspaceRoots::new(cwd, additional_directories)
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
    match worktree::prompt_remove_on_exit_menu(w) {
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

#[derive(Debug, Clone)]
struct RuntimeOptions {
    agent_stderr: Option<PathBuf>,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
}

struct RunSessionResult {
    reason: UiExitReason,
    session_id: Option<String>,
    session_title: Option<String>,
    theme_kind: theme::TerminalThemeKind,
    spinner_style: spinner::SpinnerStyle,
}

impl From<ui::UiRunResult> for RunSessionResult {
    fn from(result: ui::UiRunResult) -> Self {
        Self {
            reason: result.reason,
            session_id: result.session_id,
            session_title: result.session_title,
            theme_kind: result.theme_kind,
            spinner_style: result.spinner_style,
        }
    }
}

fn apply_session_result_to_config(cfg: &mut Config, result: &RunSessionResult) {
    cfg.theme = result.theme_kind;
    cfg.spinner = result.spinner_style;
}

async fn run_app(
    cwd: PathBuf,
    runtime_options: RuntimeOptions,
    project_label: String,
    worktree_label: Option<String>,
    resume_target: Option<ResumeTarget>,
    initial_agent: Option<SelectedAgent>,
    mode: UiMode,
) -> Result<Option<String>> {
    let config_path = config::default_config_path();
    let mut cfg = Config::load(&config_path)?;
    let mut council = council::resolve(&cfg, &cwd).await?;
    if let Some(agent) = initial_agent.as_ref()
        && let Some(pinned) = council.available.iter().find(|role| {
            role.launch.command == agent.program
                && role.launch.args == agent.args
                && role.model.model == agent.source_id.trim_start_matches("council:")
        })
    {
        council.thor = pinned.clone();
    }
    let mut council_agent = selected_agent_for_role(&council.thor);

    // Consume resume_session and any pinned resume launch on the first
    // iteration only. Fresh sessions always use the resolved Thor role.
    let mut initial_resume = resume_target;
    let mut initial_agent = initial_agent.or_else(|| Some(council_agent.clone()));
    let mut pending_new_session_boundary = false;
    loop {
        let resume = initial_resume.take();
        let agent = initial_agent
            .take()
            .unwrap_or_else(|| council_agent.clone());

        let session_boundary = new_session_boundary_for_agent(
            std::mem::take(&mut pending_new_session_boundary),
            &agent,
        );

        let session_result = run_session(
            &agent,
            cwd.clone(),
            runtime_options.clone(),
            HeaderLabels {
                project: project_label.clone(),
                worktree: worktree_label.clone(),
                additional_roots: runtime_options.additional_directories.len(),
                session_title: resume.as_ref().and_then(|target| target.title.clone()),
            },
            resume.as_ref().map(|target| target.session_id.clone()),
            mode,
            cfg.theme,
            cfg.spinner,
            session_boundary,
            council.clone(),
            cfg.thor.clone(),
            cfg.loki.clone(),
        )
        .await?;
        apply_session_result_to_config(&mut cfg, &session_result);
        match session_result.reason {
            UiExitReason::Quit => return Ok(session_result.session_id),
            UiExitReason::NewSession => {
                cfg = Config::load(&config_path)?;
                council = council::resolve(&cfg, &cwd).await?;
                council_agent = selected_agent_for_role(&council.thor);
                initial_agent = Some(council_agent.clone());
                pending_new_session_boundary = true;
                continue;
            }
            UiExitReason::ClearSession => {
                initial_agent = Some(agent);
                continue;
            }
            UiExitReason::SwitchSession => {
                if let Some(session_id) = session_result.session_id {
                    let resume_agent = session_provenance::find(&session_id, &cwd)
                        .and_then(|record| {
                            council.available.iter().find(|role| {
                                role.model.model == record.model
                                    && role.model_value == record.model_value
                                    && role.launch.source_id == record.adapter_source_id
                            })
                        })
                        .map(selected_agent_for_role)
                        .unwrap_or(agent);
                    initial_resume = Some(ResumeTarget {
                        session_id,
                        title: session_result.session_title,
                    });
                    initial_agent = Some(resume_agent);
                    continue;
                }
                return Ok(None);
            }
            UiExitReason::LoadSession => {
                match run_session_picker_action_for_agent(
                    &agent,
                    cwd.clone(),
                    runtime_options.agent_stderr.as_deref(),
                    session_result.session_id,
                    session_result.session_title,
                    cfg.theme.palette(),
                )
                .await?
                {
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

async fn run_session_picker_action_for_agent(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
    current_session_id: Option<String>,
    current_session_title: Option<String>,
    theme: palette::TerminalTheme,
) -> Result<SessionPickerAction> {
    let mut notice = None;
    loop {
        let listing =
            session::list_sessions_with_capabilities(agent, cwd.clone(), agent_stderr).await?;
        if listing.sessions.is_empty() {
            return Ok(session_picker_empty_action(
                current_session_id,
                current_session_title,
            ));
        }

        let delete_supported = in_app_session_delete_supported(
            listing.delete_supported,
            current_session_id.as_deref(),
        );
        let outcome =
            run_session_picker_once(listing.sessions, delete_supported, notice.take(), theme)
                .await?;
        if let session::ResumeOutcome::DeleteRequested(entry) = outcome {
            if current_session_id.as_deref() == Some(entry.session_id.as_str()) {
                notice = Some(
                    "Cannot delete the active session from the session picker. Close it first."
                        .to_string(),
                );
            } else {
                notice = Some(delete_session_notice(agent, entry, agent_stderr).await);
            }
            continue;
        }

        return session_picker_action(outcome, current_session_id, current_session_title);
    }
}

async fn run_session_picker_action_for_council(
    council: &council::ResolvedCouncil,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
    current_session_id: Option<String>,
    current_session_title: Option<String>,
    theme: palette::TerminalTheme,
) -> Result<(SessionPickerAction, Option<council::ResolvedRole>)> {
    let mut notice = None;
    loop {
        let sessions = list_council_sessions(council, &cwd, agent_stderr).await;
        if sessions.is_empty() {
            return Ok((
                session_picker_empty_action(current_session_id, current_session_title),
                None,
            ));
        }
        let outcome = run_session_picker_once(sessions, true, notice.take(), theme).await?;
        match outcome {
            session::ResumeOutcome::Cancelled => {
                return Ok((
                    session_picker_action(
                        session::ResumeOutcome::Cancelled,
                        current_session_id,
                        current_session_title,
                    )?,
                    None,
                ));
            }
            session::ResumeOutcome::DeleteRequested(entry) => {
                if current_session_id.as_deref() == Some(entry.session_id.as_str()) {
                    notice = Some(
                        "Cannot delete the active session from the session picker. Close it first."
                            .to_string(),
                    );
                    continue;
                }
                notice = match role_for_session_entry(council, &entry) {
                    Some(role) if entry.delete_supported => {
                        let route = selected_agent_for_role(role);
                        Some(delete_session_notice(&route, entry, agent_stderr).await)
                    }
                    Some(_) => {
                        Some("This ACP adapter does not support session deletion".to_string())
                    }
                    None => Some("Delete failed: session route is unavailable".to_string()),
                };
            }
            session::ResumeOutcome::Selected(entry) => {
                let role = role_for_session_entry(council, &entry)
                    .ok_or_else(|| anyhow::anyhow!("selected session route is unavailable"))?
                    .clone();
                session_provenance::record(session_provenance::Record {
                    session_id: entry.session_id.clone(),
                    cwd: entry.cwd.clone(),
                    adapter_source_id: role.launch.source_id.clone(),
                    model: role.model.model.clone(),
                    model_value: role.model_value.clone(),
                });
                return Ok((
                    SessionPickerAction::Resume {
                        session_id: entry.session_id,
                        title: entry.title,
                    },
                    Some(role),
                ));
            }
        }
    }
}

fn in_app_session_delete_supported(
    agent_delete_supported: bool,
    current_session_id: Option<&str>,
) -> bool {
    agent_delete_supported && current_session_id.is_some()
}

fn session_picker_empty_action(
    current_session_id: Option<String>,
    current_session_title: Option<String>,
) -> SessionPickerAction {
    match current_session_id {
        Some(session_id) => SessionPickerAction::Resume {
            session_id,
            title: current_session_title,
        },
        None => SessionPickerAction::Exit(None),
    }
}

async fn delete_session_notice(
    agent: &SelectedAgent,
    entry: session::SessionEntry,
    agent_stderr: Option<&Path>,
) -> String {
    let label = entry
        .title
        .as_deref()
        .unwrap_or(entry.session_id.as_str())
        .to_string();
    let cwd = entry.cwd.clone();
    let adapter_source_id = entry.adapter_source_id.clone();
    let session_id = entry.session_id;
    match session::delete_session(agent, session_id.clone(), agent_stderr).await {
        Ok(()) => {
            session_provenance::remove(&session_id, &cwd, adapter_source_id.as_deref());
            format!("Deleted session: {label}")
        }
        Err(err) => format!("Delete failed for {label}: {err:#}"),
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

fn new_session_boundary_for_agent(
    pending_new_session_boundary: bool,
    agent: &SelectedAgent,
) -> Option<String> {
    pending_new_session_boundary
        .then(|| format!("new {} session started", agent_header_label(agent)))
}

#[cfg(test)]
fn resume_target_after_cancelled_new_session(
    agent: SelectedAgent,
    session_id: Option<String>,
    session_title: Option<String>,
) -> (SelectedAgent, Option<ResumeTarget>) {
    let resume = session_id.map(|session_id| ResumeTarget {
        session_id,
        title: session_title,
    });
    (agent, resume)
}

fn session_picker_action(
    outcome: session::ResumeOutcome,
    current_session_id: Option<String>,
    current_session_title: Option<String>,
) -> Result<SessionPickerAction> {
    match outcome {
        session::ResumeOutcome::Selected(entry) => Ok(SessionPickerAction::Resume {
            session_id: entry.session_id,
            title: entry.title,
        }),
        session::ResumeOutcome::DeleteRequested(_) => {
            anyhow::bail!("session delete request was not handled by picker flow")
        }
        // Cancelling the picker keeps the current session running, so carry
        // its known title forward instead of dropping it — otherwise the
        // header title would blank out until the agent's next SessionInfoUpdate.
        session::ResumeOutcome::Cancelled => Ok(match current_session_id {
            Some(session_id) => SessionPickerAction::Resume {
                session_id,
                title: current_session_title,
            },
            None => SessionPickerAction::Exit(None),
        }),
    }
}

async fn run_session_picker_once(
    sessions: Vec<session::SessionEntry>,
    delete_supported: bool,
    notice: Option<String>,
    theme: palette::TerminalTheme,
) -> Result<session::ResumeOutcome> {
    let mut terminal = ui::setup_fullscreen_terminal().context("setup terminal")?;
    let outcome =
        session::run_session_picker(&mut terminal, sessions, delete_supported, notice, theme).await;
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

fn agent_header_label(agent: &SelectedAgent) -> String {
    remote::agent_display_label(agent)
}

fn selected_agent_for_role(role: &council::ResolvedRole) -> SelectedAgent {
    SelectedAgent {
        source_id: format!("council:{}", role.model.model),
        program: role.launch.command.clone(),
        args: role.launch.args.clone(),
        env: role.launch.env.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    agent: &SelectedAgent,
    cwd: PathBuf,
    runtime_options: RuntimeOptions,
    header_labels: HeaderLabels,
    resume_session: Option<String>,
    mode: UiMode,
    mut theme_kind: theme::TerminalThemeKind,
    mut spinner_style: spinner::SpinnerStyle,
    mut session_boundary: Option<String>,
    council: council::ResolvedCouncil,
    thor_config: config::ThorConfig,
    loki_config: config::LokiConfig,
) -> Result<RunSessionResult> {
    let mut terminal = SessionTerminal::fresh(mode)?;
    let (eitri_role, _eitri_codex_home) = isolated_council_role(council.eitri.clone(), "eitri")?;
    let (loki_role, _loki_codex_home) = match council.loki.clone() {
        Some(role) => {
            let (role, guard) = isolated_council_role(role, "loki")?;
            (Some(role), guard)
        }
        None => (None, None),
    };

    let (event_tx, runtime_event_rx) = mpsc::unbounded_channel();
    let (ui_event_tx, ui_event_rx) = mpsc::unbounded_channel();
    let (runtime_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel();
    let implementation_handoffs_this_turn = Arc::new(AtomicUsize::new(0));
    let _ = ui_event_tx.send(crate::event::UiEvent::Info(format!(
        "Council · Thor {} · Loki {} · Eitri {} · {} launchable models",
        council.thor.model.model,
        council
            .loki
            .as_ref()
            .map(|role| role.model.model.as_str())
            .unwrap_or("off"),
        council.eitri.model.model,
        council.available.len(),
    )));
    for warning in &council.warnings {
        let _ = ui_event_tx.send(crate::event::UiEvent::Warning(warning.clone()));
    }
    let loki_handle = loki_role.map(|role| {
        loki::Handle::start(
            role,
            cwd.clone(),
            runtime_options.additional_directories.clone(),
            ui_event_tx.clone(),
            loki_config.streaming_review,
        )
    });
    if loki_config.streaming_review && loki_handle.is_none() {
        let _ = ui_event_tx.send(crate::event::UiEvent::Warning(
            "Loki review is enabled, but no launchable model from a provider other than Thor's is available"
                .to_string(),
        ));
    }
    let (usage_turn_tx, usage_task) = if agent.source_id == "claude-acp" {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let usage_ui_tx = ui_event_tx.clone();
        let usage_cwd = cwd.clone();
        let usage_env = agent.env.clone();
        let handle = tokio::spawn(async move {
            let mut completed_turns = 0_u64;
            while rx.recv().await.is_some() {
                completed_turns = completed_turns.saturating_add(1);
                if !completed_turns.is_multiple_of(2) {
                    continue;
                }
                match claude_usage::query(usage_cwd.clone(), usage_env.clone()).await {
                    Ok(report) => {
                        let _ = usage_ui_tx.send(crate::event::UiEvent::ClaudeUsage(report));
                    }
                    Err(error) => {
                        tracing::warn!("claude /usage failed: {error}");
                    }
                }
            }
        });
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };
    let mut ui_event_rx = ui_event_rx;

    let runtime_cfg = acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd: cwd.clone(),
        additional_directories: runtime_options.additional_directories.clone(),
        mcp_servers: Vec::new(),
        resume_session,
        env: agent.env.clone(),
        agent_stderr: runtime_options.agent_stderr.clone(),
        fs_max_text_bytes: runtime_options.fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: Some(agent.source_id.clone()),
        config_path: Some(config::default_config_path()),
        saved_session_config: std::collections::HashMap::new(),
        role_config: Some(acp::RuntimeRoleConfig {
            label: "Thor".to_string(),
            model_value: council.thor.model_value.clone(),
            force_high_reasoning: true,
        }),
        code_agent: Some(
            code_agent::Config::council(
                eitri_role.launch.command.clone(),
                eitri_role.launch.args.clone(),
                eitri_role.launch.env.clone(),
                runtime_options.agent_stderr.clone(),
                eitri_role.model.model.clone(),
                eitri_role.model_value.clone(),
                loki_handle.clone(),
            )
            .with_implementation_handoff_counter(implementation_handoffs_this_turn.clone()),
        ),
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
    let config_path = config::default_config_path();
    // Pre-fill the UI header with the immutable model selected for this session.
    let agent_display_name = Some(format!("Thor · {}", council.thor.model.model));
    // Stable runtime route identifier used by remote session state.
    let agent_source_id = Some(agent.source_id.clone());
    let tracker_project_label = header_labels.project.clone();
    // `-w` sessions carry the worktree name in the header; sessions launched
    // directly inside a worktree derive it from cwd so remote viewers badge
    // both the same way.
    let tracker_worktree_label = header_labels
        .worktree
        .clone()
        .or_else(|| paths::worktree_name_from_cwd(&cwd));
    let remote_tracker = remote::RemoteSessionTracker::new(
        tracker_project_label,
        tracker_worktree_label,
        format!("Thor · {}", council.thor.model.model),
        Some(cmd_tx.clone()),
        Some(ui_event_tx.clone()),
    );

    let event_tracker = remote_tracker.clone();
    let review_commands = runtime_cmd_tx.clone();
    let turn_state = std::sync::Arc::new(tokio::sync::Mutex::new((
        0_u64,
        String::new(),
        None::<workspace_snapshot::WorkspaceSnapshot>,
    )));
    let event_turn_state = turn_state.clone();
    let event_loki = loki_handle.clone();
    let event_handoffs = implementation_handoffs_this_turn.clone();
    let event_thor = council.thor.clone();
    let event_cwd = cwd.clone();
    let thor_review_enabled = Arc::new(AtomicBool::new(thor_config.discrete_review));
    let event_thor_review_enabled = thor_review_enabled.clone();
    let event_proxy = tokio::spawn(async move {
        let mut runtime_event_rx = runtime_event_rx;
        let mut decisions = event_loki.as_ref().map(loki::Handle::subscribe);
        let mut trajectory = loki::BoundaryTracker::default();
        let mut pending = std::collections::HashSet::new();
        let mut held_completion = None;
        let mut intervention = loki::DeferredIntervention::default();
        let mut discrete_review_started = false;
        loop {
            tokio::select! {
                event = runtime_event_rx.recv() => {
                    let Some(event) = event else { break; };
                    if let UiEvent::SessionStarted { session_id, .. } = &event {
                        session_provenance::record(session_provenance::Record {
                            session_id: session_id.clone(),
                            cwd: event_cwd.clone(),
                            adapter_source_id: event_thor.launch.source_id.clone(),
                            model: event_thor.model.model.clone(),
                            model_value: event_thor.model_value.clone(),
                        });
                    }
                    // Intercept before rendering: permission prompts get their
                    // responder wrapped so remote viewers can answer them.
                    let event = event_tracker.intercept_event(event);
                    let (epoch, task, _) = event_turn_state.lock().await.clone();
                    let boundary = (epoch > 0).then(|| trajectory.observe(&event)).flatten();
                    let boundary_observed = boundary.is_some();
                    let target_completed = council_target_completed(&event);
                    let interrupting = boundary_observed
                        && !target_completed
                        && intervention.interrupt_at_boundary();
                    if interrupting {
                        let _ = ui_event_tx.send(crate::event::UiEvent::Info(
                            "Thor · interrupting at step boundary for Loki review".to_string(),
                        ));
                        let _ = review_commands.send(UiCommand::CancelPrompt);
                    }
                    if let Some(boundary) = boundary
                        && !interrupting
                        && !(target_completed && intervention.is_pending())
                        && let Some(reviewer) = event_loki.as_ref()
                        && let Some(id) = reviewer
                            .observe(epoch, loki::Target::Thor, boundary)
                            .await
                    {
                        pending.insert(id);
                    }

                    if let crate::event::UiEvent::PromptDone { stop_reason, .. } = &event {
                        let cancelled = matches!(
                            stop_reason,
                            agent_client_protocol::schema::v1::StopReason::Cancelled
                        );
                        if cancelled
                            && intervention.is_pending()
                            && !intervention.cancellation_was_requested()
                        {
                            // Do not turn a user's cancellation into a Loki
                            // continuation before the deferred boundary arrives.
                            intervention.clear();
                            pending.clear();
                        } else if let Some(critique) = intervention.take() {
                            pending.clear();
                            held_completion = None;
                            trajectory.reset_attempt();
                            let _ = ui_event_tx.send(crate::event::UiEvent::Info(
                                "Thor · resumed after Loki intervention".to_string(),
                            ));
                            let continuation = council_continuation_prompt(
                                &task,
                                &critique,
                                &trajectory.trajectory(),
                            );
                            emit_internal_message(
                                &ui_event_tx,
                                "Loki",
                                "Thor",
                                InternalMessageKind::Continuation,
                                &critique,
                            );
                            let _ = review_commands.send(UiCommand::SendPrompt {
                                text: continuation,
                                images: Vec::new(),
                            });
                            continue;
                        }
                        if cancelled {
                            event_tracker.observe_event(&event);
                            if ui_event_tx.send(event).is_err() { break; }
                            event_turn_state.lock().await.2 = None;
                            discrete_review_started = false;
                            trajectory = loki::BoundaryTracker::default();
                            continue;
                        }
                        held_completion = Some(event);
                    } else if let Some(critique) =
                        take_failed_prompt_intervention(&event, &mut intervention)
                    {
                        pending.clear();
                        held_completion = None;
                        trajectory.reset_attempt();
                        let _ = ui_event_tx.send(crate::event::UiEvent::Info(
                            "Thor · resumed after Loki intervention".to_string(),
                        ));
                        let continuation = council_continuation_prompt(
                            &task,
                            &critique,
                            &trajectory.trajectory(),
                        );
                        emit_internal_message(
                            &ui_event_tx,
                            "Loki",
                            "Thor",
                            InternalMessageKind::Continuation,
                            &critique,
                        );
                        let _ = review_commands.send(UiCommand::SendPrompt {
                            text: continuation,
                            images: Vec::new(),
                        });
                        continue;
                    } else {
                        event_tracker.observe_event(&event);
                        if ui_event_tx.send(event).is_err() { break; }
                        if target_completed {
                            event_turn_state.lock().await.2 = None;
                            discrete_review_started = false;
                            intervention.clear();
                            trajectory = loki::BoundaryTracker::default();
                        }
                    }
                }
                decision = async {
                    match decisions.as_mut() {
                        Some(rx) => rx.recv().await.ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(decision) = decision else { continue; };
                    let (epoch, task, _) = event_turn_state.lock().await.clone();
                    if decision.epoch != epoch || decision.target != loki::Target::Thor || !pending.remove(&decision.id) {
                        continue;
                    }
                    if let loki::Verdict::Intervention(critique) = decision.verdict {
                        intervention.push(decision.id, critique);
                        if held_completion.is_some() {
                            held_completion = None;
                            pending.clear();
                            trajectory.reset_attempt();
                            let critique = intervention.take().expect("intervention queued");
                            let _ = ui_event_tx.send(crate::event::UiEvent::Info(
                                "Thor · re-prompted after Loki intervention".to_string(),
                            ));
                            let continuation = council_continuation_prompt(
                                &task,
                                &critique,
                                &trajectory.trajectory(),
                            );
                            emit_internal_message(
                                &ui_event_tx,
                                "Loki",
                                "Thor",
                                InternalMessageKind::Continuation,
                                &critique,
                            );
                            let _ = review_commands.send(UiCommand::SendPrompt {
                                text: continuation,
                                images: Vec::new(),
                            });
                        } else {
                            let _ = ui_event_tx.send(crate::event::UiEvent::Info(
                                "Thor · Loki intervention queued for the next step boundary".to_string(),
                            ));
                        }
                    }
                }
            }

            if held_completion.is_some() && pending.is_empty() && !intervention.is_pending() {
                let implementation_handoffs = event_handoffs.load(Ordering::Acquire);
                let discrete_review_enabled = event_thor_review_enabled.load(Ordering::Acquire);
                let workspace_delta = if discrete_review_enabled
                    && implementation_handoffs > 1
                    && !discrete_review_started
                {
                    let (_, _, snapshot) = event_turn_state.lock().await.clone();
                    match snapshot {
                        Some(snapshot) => Some(snapshot.delta().await),
                        None => None,
                    }
                } else {
                    None
                };
                let workspace_changed = workspace_delta
                    .as_ref()
                    .is_some_and(workspace_snapshot::WorkspaceDelta::changed);
                if should_start_discrete_review(
                    discrete_review_enabled,
                    discrete_review_started,
                    implementation_handoffs,
                    workspace_changed,
                ) {
                    let (epoch, task, _) = event_turn_state.lock().await.clone();
                    if epoch == 0 {
                        continue;
                    }
                    let initial_result = trajectory.final_message();
                    let context =
                        discrete_review_context(workspace_delta.as_ref(), trajectory.trajectory());
                    held_completion = None;
                    trajectory.reset_attempt();
                    discrete_review_started = true;
                    let _ = ui_event_tx.send(crate::event::UiEvent::Info(
                        "reviewing the completed work…".to_string(),
                    ));
                    let review_prompt =
                        thor_discrete_review_prompt(&task, &initial_result, &context);
                    emit_internal_message(
                        &ui_event_tx,
                        "Thor",
                        "Thor",
                        InternalMessageKind::DiscreteReview,
                        &review_prompt,
                    );
                    let _ = review_commands.send(UiCommand::SendPrompt {
                        text: review_prompt,
                        images: Vec::new(),
                    });
                    continue;
                }
                let event = held_completion.take().expect("completion held");
                event_tracker.observe_event(&event);
                if let Some(tx) = usage_turn_tx.as_ref() {
                    let _ = tx.send(());
                }
                if ui_event_tx.send(event).is_err() {
                    break;
                }
                event_turn_state.lock().await.2 = None;
                discrete_review_started = false;
                intervention.clear();
                trajectory = loki::BoundaryTracker::default();
            }
        }
    });

    let cmd_tracker = remote_tracker.clone();
    let cmd_loki = loki_handle.clone();
    let cmd_thor_review_enabled = thor_review_enabled.clone();
    let cmd_turn_state = turn_state.clone();
    let mut cmd_workspace_roots =
        Vec::with_capacity(1 + runtime_options.additional_directories.len());
    cmd_workspace_roots.push(cwd.clone());
    cmd_workspace_roots.extend(runtime_options.additional_directories.iter().cloned());
    let cmd_proxy = tokio::spawn(async move {
        let mut local_epoch = 0_u64;
        while let Some(command) = ui_cmd_rx.recv().await {
            cmd_tracker.observe_command(&command);
            if let UiCommand::SetReviewPolicy { role, enabled } = &command {
                match role {
                    ReviewRole::Thor => {
                        cmd_thor_review_enabled.store(*enabled, Ordering::Release);
                    }
                    ReviewRole::Loki => {
                        if let Some(reviewer) = cmd_loki.as_ref() {
                            reviewer.set_streaming_enabled(*enabled);
                            if !*enabled {
                                reviewer.cancel_turn();
                            }
                        }
                    }
                }
                continue;
            }
            if let UiCommand::SendPrompt { text, .. } = &command {
                local_epoch = local_epoch.saturating_add(1);
                implementation_handoffs_this_turn.store(0, Ordering::Release);
                let snapshot =
                    workspace_snapshot::WorkspaceSnapshot::capture(&cmd_workspace_roots).await;
                let epoch = cmd_loki
                    .as_ref()
                    .map_or(local_epoch, |reviewer| reviewer.begin_turn(text.clone()));
                *cmd_turn_state.lock().await = (epoch, text.clone(), Some(snapshot));
            }
            if matches!(command, UiCommand::CancelPrompt)
                && let Some(reviewer) = cmd_loki.as_ref()
            {
                reviewer.cancel_turn();
            }
            let shutdown = matches!(command, UiCommand::Shutdown);
            if runtime_cmd_tx.send(command).is_err() || shutdown {
                break;
            }
        }
    });

    let mut header_labels = header_labels;
    let ui_result = loop {
        let ui_result = ui::run(
            &mut terminal.term,
            &cmd_tx,
            &mut ui_event_rx,
            header_labels.clone(),
            agent_display_name.clone(),
            agent_source_id.clone(),
            ui::UiRunOptions {
                persistence: ui::UiPersistencePaths {
                    history_path: Some(&hist_path),
                    transcript_export_dir: export_dir.as_deref(),
                    config_path: Some(&config_path),
                },
                mode,
                theme_kind,
                spinner_style,
                active_agent_launch: Some(ragnarok::Launch {
                    program: agent.program.clone(),
                    args: agent.args.clone(),
                    env: agent.env.clone(),
                }),
                session_boundary: session_boundary.take(),
                session_cwd: cwd.clone(),
                council_choices: council.choices.clone(),
                council_models: config::Config::load(&config_path)
                    .map(|config| config.role_models())
                    .unwrap_or_default(),
                active_council_models: config::ModelsConfig {
                    thor: council.thor.model.model.clone(),
                    loki: council
                        .loki
                        .as_ref()
                        .map(|role| role.model.model.clone())
                        .unwrap_or_else(|| "off".to_string()),
                    eitri: council.eitri.model.model.clone(),
                },
                thor_review_enabled: thor_config.discrete_review,
                loki_review_enabled: loki_config.streaming_review,
                ragnarok_models: council.available.clone(),
                primary_acp_name: council.thor.launch.kind.display_name().to_string(),
            },
        )
        .await;

        // Adopt any theme/spinner the user changed during the session so the
        // picker and any follow-on session inherit them.
        if let Ok(result) = ui_result.as_ref() {
            theme_kind = result.theme_kind;
            spinner_style = result.spinner_style;
        }

        // Only the session picker (LoadSession) needs the active session UI
        // torn down before it draws. Every other outcome — quit, /new, /clear,
        // or an error — keeps the session UI on screen (the inline prompt, or
        // the fullscreen alt-screen) while the runtime shuts down below; the
        // terminal is restored just before we return, so the user never watches
        // a cleared viewport or a bare primary buffer during teardown.
        let result = match ui_result {
            Ok(result) if result.reason == UiExitReason::LoadSession => result,
            other => break other.map(Into::into),
        };

        // LoadSession: restore now so the fullscreen session picker can take
        // over the screen.
        terminal.restore_once(mode);

        let current_session_id = result.session_id;
        let current_session_title = result.session_title;

        let (action, selected_role) = match run_session_picker_action_for_council(
            &council,
            cwd.clone(),
            runtime_options.agent_stderr.as_deref(),
            current_session_id.clone(),
            current_session_title.clone(),
            theme_kind.palette(),
        )
        .await
        {
            Ok(action) => action,
            Err(e) => {
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break Err(e);
            }
        };
        let SessionPickerAction::Resume {
            session_id: target_session_id,
            title: target_title,
        } = action
        else {
            let _ = cmd_tx.send(UiCommand::Shutdown);
            break Ok(RunSessionResult {
                reason: UiExitReason::Quit,
                session_id: current_session_id,
                session_title: current_session_title,
                theme_kind,
                spinner_style,
            });
        };

        if selected_role.as_ref().is_some_and(|role| {
            role.launch.source_id != council.thor.launch.source_id
                || role.model.model != council.thor.model.model
        }) {
            let _ = cmd_tx.send(UiCommand::Shutdown);
            break Ok(RunSessionResult {
                reason: UiExitReason::SwitchSession,
                session_id: Some(target_session_id),
                session_title: target_title,
                theme_kind,
                spinner_style,
            });
        }

        match request_inline_session_load(
            &cmd_tx,
            target_session_id.clone(),
            cwd.clone(),
            target_title.clone(),
        )
        .await
        {
            LoadSessionResult::Switched => {
                header_labels.session_title = target_title;
                // A fresh terminal starts unrestored, so the exit path will
                // restore it again — no manual bookkeeping needed.
                terminal = match SessionTerminal::fresh(mode) {
                    Ok(terminal) => terminal,
                    Err(e) => {
                        let _ = cmd_tx.send(UiCommand::Shutdown);
                        break Err(e);
                    }
                };
                continue;
            }
            LoadSessionResult::Fallback { message } => {
                tracing::info!("falling back to restart-based session load: {message}");
                let _ = cmd_tx.send(UiCommand::Shutdown);
                break Ok(RunSessionResult {
                    reason: UiExitReason::SwitchSession,
                    session_id: Some(target_session_id),
                    session_title: target_title,
                    theme_kind,
                    spinner_style,
                });
            }
        }
    };

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
    remote_tracker.shutdown().await;
    if let Some(reviewer) = loki_handle.as_ref() {
        reviewer.shutdown_and_wait().await;
    }

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
    if let Some(task) = usage_task {
        wait_for_task("claude usage poller", task).await;
    }

    // Restore the terminal only now, after the runtime has finished tearing
    // down, so the session UI stayed on screen through shutdown and is torn
    // down moments before the process exits (or the next session draws) instead
    // of leaving a blank gap during teardown. No-op if the LoadSession path
    // already restored before showing the session picker.
    terminal.restore_once(mode);
    if matches!(
        ui_result.as_ref().map(|result| result.reason),
        Ok(UiExitReason::ClearSession)
    ) && let Err(e) = ui::clear_terminal_screen(&mut terminal.term)
    {
        tracing::warn!("clear terminal for /clear failed: {e}");
    }

    ui_result
}

fn isolated_council_role(
    mut role: council::ResolvedRole,
    label: &str,
) -> Result<(council::ResolvedRole, Option<tempfile::TempDir>)> {
    if role.launch.kind != council::AdapterKind::Codex {
        return Ok((role, None));
    }
    let source = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| anyhow::anyhow!("could not locate CODEX_HOME for {label}"))?;
    let isolated = tempfile::Builder::new()
        .prefix(&format!("mj-{label}-codex-"))
        .tempdir()
        .with_context(|| format!("create isolated Codex home for {label}"))?;
    for name in [
        "auth.json",
        "config.toml",
        "models_cache.json",
        "version.json",
    ] {
        let from = source.join(name);
        if from.is_file() {
            std::fs::copy(&from, isolated.path().join(name)).with_context(|| {
                format!("copy {} into isolated {label} Codex home", from.display())
            })?;
        }
    }
    if !isolated.path().join("auth.json").exists() {
        anyhow::bail!(
            "Codex is available but {} has no auth.json; run `codex login`",
            source.display()
        );
    }
    role.launch.env.insert(
        "CODEX_HOME".to_string(),
        isolated.path().display().to_string(),
    );
    Ok((role, Some(isolated)))
}

fn council_continuation_prompt(_task: &str, critique: &str, _trajectory: &str) -> String {
    format!(
        "<advisory guidance=\"weigh, don't blindly obey\">\n{critique}\n</advisory>\n\nContinue the interrupted turn. Address the material advice, then finish the existing task. Please continue from where you left off."
    )
}

fn should_start_discrete_review(
    enabled: bool,
    already_started: bool,
    implementation_handoffs: usize,
    workspace_changed: bool,
) -> bool {
    enabled && !already_started && implementation_handoffs > 1 && workspace_changed
}

fn emit_internal_message(
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    source: &str,
    target: &str,
    kind: InternalMessageKind,
    text: &str,
) {
    let _ = ui_tx.send(UiEvent::InternalMessage(InternalMessage {
        source: source.to_string(),
        target: target.to_string(),
        kind,
        text: text.to_string(),
    }));
}

fn council_target_completed(event: &crate::event::UiEvent) -> bool {
    matches!(
        event,
        crate::event::UiEvent::PromptDone { .. } | crate::event::UiEvent::PromptFailed { .. }
    )
}

fn take_failed_prompt_intervention(
    event: &crate::event::UiEvent,
    intervention: &mut loki::DeferredIntervention,
) -> Option<String> {
    matches!(event, crate::event::UiEvent::PromptFailed { .. })
        .then(|| intervention.take())
        .flatten()
}

fn thor_discrete_review_prompt(task: &str, initial_result: &str, context: &str) -> String {
    format!(
        "Perform Thor's discrete review for this same user turn. You own the research, planning, coordination, review, verification, and final response; do not act as a thin relay for Eitri. Re-read the original task, critically review the initial result and implementation evidence, investigate or verify anything necessary, and correct material issues. If code changes are still needed, delegate them to Eitri with code_agent and then review the new result. Return the final user-facing answer when the work is genuinely complete.\n\nOriginal task:\n{task}\n\nInitial result:\n{initial_result}\n\nBounded trajectory and workspace context:\n{context}"
    )
}

fn discrete_review_context(
    delta: Option<&workspace_snapshot::WorkspaceDelta>,
    trajectory: String,
) -> String {
    let diff = match delta {
        Some(delta) => delta
            .review_patch()
            .map(str::to_string)
            .unwrap_or_else(|| "[no workspace changes attributable to this user turn]".to_string()),
        None => "[workspace turn snapshot unavailable]".to_string(),
    };
    let mut context = format!("Trajectory:\n{trajectory}\n\nWorkspace diff:\n{diff}");
    const LIMIT: usize = 128 * 1024;
    if context.len() > LIMIT {
        let split = context.ceil_char_boundary(context.len() - LIMIT);
        context = format!("…[earlier review context omitted]\n{}", &context[split..]);
    }
    context
}

fn setup_session_terminal(
    mode: UiMode,
) -> Result<ratatui::Terminal<crate::term::TrackedBackend<std::io::Stdout>>> {
    match mode {
        UiMode::InlineChat => {
            ui::setup_inline_chat_terminal(ui::INLINE_CHAT_HEIGHT).context("setup terminal")
        }
        UiMode::FullscreenTui => ui::setup_fullscreen_terminal().context("setup terminal"),
    }
}

fn restore_session_terminal(
    terminal: &mut ratatui::Terminal<crate::term::TrackedBackend<std::io::Stdout>>,
    mode: UiMode,
) -> Result<()> {
    match mode {
        UiMode::InlineChat => ui::restore_inline_chat_terminal(terminal),
        UiMode::FullscreenTui => ui::restore_fullscreen_terminal(terminal),
    }
}

/// The session terminal paired with whether it has already been restored.
///
/// `run_session` must restore the terminal exactly once, but the moment varies
/// by exit path (the LoadSession picker needs it restored eagerly; every other
/// path defers until after runtime teardown). Binding the flag to the terminal
/// value keeps the two in sync by construction: `fresh` always starts
/// unrestored, and `restore_once` is idempotent, so every exit path can call it
/// without tracking who restored first.
struct SessionTerminal {
    term: ratatui::Terminal<crate::term::TrackedBackend<std::io::Stdout>>,
    restored: bool,
}

impl SessionTerminal {
    fn fresh(mode: UiMode) -> Result<Self> {
        Ok(Self {
            term: setup_session_terminal(mode)?,
            restored: false,
        })
    }

    /// Restore the terminal if it hasn't been already; later calls are no-ops.
    fn restore_once(&mut self, mode: UiMode) {
        if self.restored {
            return;
        }
        if let Err(e) = restore_session_terminal(&mut self.term, mode) {
            tracing::warn!("restore terminal failed: {e}");
        }
        self.restored = true;
    }
}

async fn request_inline_session_load(
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    session_id: String,
    cwd: PathBuf,
    title: Option<String>,
) -> LoadSessionResult {
    let (responder, response) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(UiCommand::LoadSession {
            session_id,
            cwd,
            title,
            responder,
        })
        .is_err()
    {
        return LoadSessionResult::Fallback {
            message: "ACP runtime command channel closed".to_string(),
        };
    }
    match tokio::time::timeout(Duration::from_secs(15), response).await {
        Ok(Ok(result)) => result,
        Ok(Err(_closed)) => LoadSessionResult::Fallback {
            message: "ACP runtime closed before session switch completed".to_string(),
        },
        Err(_elapsed) => LoadSessionResult::Fallback {
            message: "ACP runtime did not complete session switch within 15s".to_string(),
        },
    }
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
    fn prompt_failure_resolves_deferred_intervention_for_current_turn() {
        let event = crate::event::UiEvent::PromptFailed {
            message: "prompt transport failed".to_string(),
        };
        let mut trajectory = loki::BoundaryTracker::default();
        let mut intervention = loki::DeferredIntervention::default();
        intervention.push(7, "retry with the corrected result".to_string());

        let boundary_observed = trajectory.observe(&event).is_some();
        let target_completed = council_target_completed(&event);
        let interrupting =
            boundary_observed && !target_completed && intervention.interrupt_at_boundary();

        assert!(target_completed);
        assert!(!interrupting);
        assert!(!intervention.cancellation_was_requested());

        let critique = take_failed_prompt_intervention(&event, &mut intervention)
            .expect("queued intervention should resolve at prompt failure");
        let continuation =
            council_continuation_prompt("current user turn", &critique, &trajectory.trajectory());

        assert!(!intervention.is_pending());
        assert!(continuation.contains("<advisory"));
        assert!(continuation.contains("retry with the corrected result"));
        assert!(continuation.contains("Please continue from where you left off."));
        assert!(!continuation.contains("current user turn"));
        assert!(!continuation.contains("Bounded prior trajectory"));
    }

    #[test]
    fn discrete_review_is_thors_review_and_preserves_eitri_delegation() {
        let prompt = thor_discrete_review_prompt(
            "implement the feature",
            "Eitri says it is complete",
            "Trajectory and diff",
        );

        assert!(prompt.contains("Perform Thor's discrete review"));
        assert!(prompt.contains("do not act as a thin relay for Eitri"));
        assert!(prompt.contains("research, planning, coordination, review, verification"));
        assert!(prompt.contains("delegate them to Eitri with code_agent"));
        assert!(prompt.contains("Initial result:\nEitri says it is complete"));
        assert!(!prompt.contains("Loki verdict"));
    }

    #[test]
    fn discrete_review_requires_multiple_eitri_handoffs_and_workspace_changes() {
        assert!(should_start_discrete_review(true, false, 2, true));
        assert!(!should_start_discrete_review(true, false, 0, true));
        assert!(!should_start_discrete_review(true, false, 1, true));
        assert!(!should_start_discrete_review(true, false, 2, false));
        assert!(!should_start_discrete_review(false, false, 2, true));
        assert!(!should_start_discrete_review(true, true, 2, true));
    }

    #[test]
    fn agent_header_label_uses_adapter_source_id() {
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
    fn new_session_boundary_uses_selected_agent_label_only_when_pending() {
        let agent = SelectedAgent {
            source_id: "claude-acp".to_string(),
            program: PathBuf::from("npx"),
            args: vec!["-y".to_string(), "@x/claude".to_string()],
            env: Default::default(),
        };

        assert_eq!(
            new_session_boundary_for_agent(true, &agent),
            Some("new claude-acp session started".to_string())
        );
        assert_eq!(new_session_boundary_for_agent(false, &agent), None);
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
    fn session_result_updates_supervisor_theme_before_next_action() {
        let mut cfg = Config::default();
        let result = RunSessionResult {
            reason: UiExitReason::ClearSession,
            session_id: Some("session-1".to_string()),
            session_title: Some("Current".to_string()),
            theme_kind: theme::TerminalThemeKind::AnsiLight,
            spinner_style: spinner::SpinnerStyle::Bars,
        };

        apply_session_result_to_config(&mut cfg, &result);

        assert_eq!(cfg.theme, theme::TerminalThemeKind::AnsiLight);
        assert_eq!(cfg.spinner, spinner::SpinnerStyle::Bars);
    }

    #[test]
    fn cancelled_new_session_picker_resumes_current_session() {
        let agent = SelectedAgent {
            source_id: "claude-acp".to_string(),
            program: PathBuf::from("npx"),
            args: vec!["-y".to_string(), "@x/claude".to_string()],
            env: Default::default(),
        };

        let (selected_agent, resume) = resume_target_after_cancelled_new_session(
            agent.clone(),
            Some("current-session".to_string()),
            Some("Current title".to_string()),
        );

        assert_eq!(selected_agent, agent);
        assert_eq!(
            resume,
            Some(ResumeTarget {
                session_id: "current-session".to_string(),
                title: Some("Current title".to_string()),
            })
        );
    }

    #[test]
    fn parse_accepts_debug_file_aliases() {
        let cli = Cli::try_parse_from(["mj", "--debug-file", "debug.log"]).expect("parse");
        assert_eq!(cli.log_file, Some(PathBuf::from("debug.log")));

        let cli = Cli::try_parse_from(["mj", "--log-file", "legacy.log"]).expect("parse");
        assert_eq!(cli.log_file, Some(PathBuf::from("legacy.log")));
    }

    #[test]
    fn parse_accepts_filesystem_text_limit() {
        let cli = Cli::try_parse_from(["mj", "--fs-max-text-bytes", "4096"]).expect("parse");
        assert_eq!(cli.fs_max_text_bytes, 4096);

        let cli = Cli::try_parse_from([
            "mj",
            "--fs-max-text-bytes",
            &acp::MAX_CONFIGURABLE_FS_TEXT_BYTES.to_string(),
        ])
        .expect("parse max");
        assert_eq!(cli.fs_max_text_bytes, acp::MAX_CONFIGURABLE_FS_TEXT_BYTES);

        let cli = Cli::try_parse_from(["mj", "server", "--fs-max-text-bytes", "8192"])
            .expect("parse server");
        assert_eq!(cli.fs_max_text_bytes, 8192);
    }

    #[test]
    fn parse_rejects_unsafe_filesystem_text_limit() {
        let err = Cli::try_parse_from(["mj", "--fs-max-text-bytes", "0"]).expect_err("reject 0");
        assert!(
            err.to_string()
                .contains("filesystem text byte limit must be between 1")
        );

        let too_large = (acp::MAX_CONFIGURABLE_FS_TEXT_BYTES + 1).to_string();
        let err = Cli::try_parse_from(["mj", "--fs-max-text-bytes", &too_large])
            .expect_err("reject too large");
        assert!(
            err.to_string()
                .contains("filesystem text byte limit must be between 1")
        );
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
        assert!(help.contains("--fs-max-text-bytes <FS_MAX_TEXT_BYTES>"));
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
            Some(Commands::Server(args)) => {
                assert!(args.hostname.is_none());
                assert!(!args.tailscale);
                assert_eq!(args.session_ttl_days, 30);
                assert!(!args.logout_all);
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_server_subcommand_with_session_flags() {
        let cli = Cli::try_parse_from(["mj", "server", "--session-ttl-days", "7", "--logout-all"])
            .expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert_eq!(args.session_ttl_days, 7);
                assert!(args.logout_all);
            }
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
    fn parse_server_subcommand_with_tailscale() {
        let cli = Cli::try_parse_from(["mj", "server", "--tailscale"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert!(args.tailscale);
                assert!(args.hostname.is_none());
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_server_rejects_tailscale_with_hostname() {
        let error =
            Cli::try_parse_from(["mj", "server", "--tailscale", "--hostname", "example.com"])
                .expect_err("conflicting flags");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
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
    fn parse_additional_directories_for_new_and_resume_sessions() {
        let cli = Cli::try_parse_from([
            "mj",
            "--additional-directory",
            "/tmp/one",
            "--add-dir",
            "/tmp/two",
        ])
        .expect("parse");
        assert_eq!(
            cli.additional_directories,
            vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]
        );

        let cli = Cli::try_parse_from([
            "mj",
            "resume",
            "sess-123",
            "--additional-directory",
            "/tmp/extra",
        ])
        .expect("parse resume");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(
                args.additional_directories,
                vec![PathBuf::from("/tmp/extra")]
            );
        } else {
            panic!("expected Resume subcommand");
        }

        let cli = Cli::try_parse_from(["mj", "--add-dir", "/tmp/top", "resume", "sess-123"])
            .expect("parse top-level add-dir before resume");
        assert_eq!(cli.additional_directories, vec![PathBuf::from("/tmp/top")]);
    }

    #[test]
    fn validate_workspace_roots_canonicalizes_and_deduplicates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let primary = tempfile::tempdir().expect("primary");
        let canonical = std::fs::canonicalize(temp.path()).expect("canonical");

        let validated = validate_workspace_roots(
            primary.path(),
            &[temp.path().to_path_buf(), canonical.clone()],
        )
        .expect("validated");

        assert_eq!(validated.additional_directories(), &[canonical]);
    }

    #[test]
    fn validate_workspace_roots_deduplicates_additional_roots_against_cwd() {
        let primary = tempfile::tempdir().expect("primary");
        let validated = validate_workspace_roots(primary.path(), &[primary.path().to_path_buf()])
            .expect("validated");

        assert!(validated.additional_directories().is_empty());
    }

    #[test]
    fn validate_workspace_roots_rejects_relative_missing_and_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let primary = tempfile::tempdir().expect("primary");
        let file = temp.path().join("file.txt");
        std::fs::write(&file, "not a directory").expect("write file");

        assert!(validate_workspace_roots(primary.path(), &[PathBuf::from("relative")]).is_err());
        assert!(validate_workspace_roots(primary.path(), &[temp.path().join("missing")]).is_err());
        assert!(validate_workspace_roots(primary.path(), &[file]).is_err());
    }

    #[test]
    fn resume_hint_includes_worktree_and_shell_quoted_additional_roots() {
        let command = resume_hint_command(
            "sess-123",
            Some("named tree"),
            &[
                PathBuf::from("/tmp/extra root"),
                PathBuf::from("/tmp/quote'root"),
            ],
        );

        assert_eq!(
            command,
            "mj resume sess-123 --worktree 'named tree' --additional-directory '/tmp/extra root' --additional-directory '/tmp/quote'\\''root'"
        );
    }

    #[test]
    fn resume_hint_leads_with_newline_in_inline_mode_only() {
        // Inline teardown leaves the cursor on the host shell's prompt row, so
        // the hint must start on a fresh line to survive the shell repaint.
        let inline = resume_hint_output(UiMode::InlineChat, "sess-123", None, &[]);
        assert_eq!(inline, "\nTo resume: mj resume sess-123");

        // Fullscreen restores via the primary buffer and needs no lead.
        let fullscreen = resume_hint_output(UiMode::FullscreenTui, "sess-123", None, &[]);
        assert_eq!(fullscreen, "To resume: mj resume sess-123");
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
        )
        .expect("cancel should resume current session");

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
        let action = session_picker_action(session::ResumeOutcome::Cancelled, None, None)
            .expect("cancel without current session should exit");

        assert_eq!(action, SessionPickerAction::Exit(None));
    }

    #[test]
    fn in_app_session_delete_requires_known_current_session_id() {
        assert!(!in_app_session_delete_supported(true, None));
        assert!(!in_app_session_delete_supported(
            false,
            Some("current-session")
        ));
        assert!(in_app_session_delete_supported(
            true,
            Some("current-session")
        ));
    }

    #[test]
    fn unhandled_delete_request_is_an_error() {
        let err = session_picker_action(
            session::ResumeOutcome::DeleteRequested(session::SessionEntry {
                session_id: "delete-me".into(),
                cwd: PathBuf::from("/tmp/project"),
                title: None,
                updated_at: None,
                adapter_source_id: None,
                model: None,
                delete_supported: false,
            }),
            Some("current-session".to_string()),
            Some("Current title".to_string()),
        )
        .expect_err("delete outcomes must be handled before action conversion");

        assert!(err.to_string().contains("delete request was not handled"));
    }

    #[test]
    fn empty_session_picker_resumes_current_session_preserving_title() {
        let action = session_picker_empty_action(
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
    fn empty_session_picker_without_current_session_exits() {
        let action = session_picker_empty_action(None, None);

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
                adapter_source_id: None,
                model: None,
                delete_supported: false,
            }),
            Some("current-session".to_string()),
            Some("ignored current title".to_string()),
        )
        .expect("select should resume selected session");

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

//! mjolnir: an interactive terminal client for any ACP-speaking agent.
//!
//! Resolves a model-first agent roster from DeepSWE and locally
//! launchable ACP adapters, then renders the active foreground ACP session in
//! a ratatui chat UI.

mod acp;
mod agent_instructions;
mod agent_usage;
mod anvil;
mod app;
mod archive;
mod auth;
mod bedrock_credits;
mod claude_usage;
mod clipboard;
mod codex_usage;
mod config;
mod deepseek_balance;
mod deepswe;
mod discrete_review;
mod event;
mod headless;
mod install;
mod kimi;
mod labels;
mod menu;
mod model_resolve;
mod notifications;
mod onboarding;
mod openrouter_balance;
mod orchestrator;
mod palette;
mod paths;
mod probe;
mod probe_cache;
mod pull_request;
mod qr;
mod quota;
mod ragnarok;
mod ragnarok_sprites;
mod registry;
mod remote;
mod roster;
mod self_update;
mod session;
mod session_provenance;
mod settings;
mod speech;
mod spinner;
mod subagent;
mod subscription;
mod tailscale;
mod term;
mod termination;
mod text;
mod theme;
mod trajectory;
mod ui;
mod usage_format;
mod version;
mod workflow;
mod workspace_snapshot;
mod worktree;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::UiExitReason;
use crate::config::{Config, SelectedAgent, history_path, transcript_export_dir};
use crate::event::{LoadSessionResult, UiCommand, UiEvent};
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
    #[arg(
        short = 'p',
        long = "print",
        value_name = "PROMPT",
        num_args = 0..=1,
        default_missing_value = "-",
        allow_hyphen_values = true
    )]
    print: Option<String>,

    /// Override the primary agent's model for this non-interactive invocation.
    ///
    /// Accepts an optional trailing `+<effort>` (off, none, minimal, low,
    /// medium, high, xhigh, max) to set this seat's ACP reasoning effort
    /// independent of the shared Anvil server default, e.g.
    /// `custom/bpr-agent/bedrock::openai.gpt-5.6-sol+high`.
    #[arg(long, value_name = "MODEL[+EFFORT]", requires = "print", value_parser = parse_model_override)]
    model: Option<(String, Option<String>)>,

    /// Override the discrete review supervisor's model for this
    /// non-interactive invocation.
    ///
    /// Accepts an optional trailing `+<effort>` on the model, same as
    /// `--model`. The review supervisor cannot be disabled independently;
    /// use the saved review toggle for that.
    #[arg(long, value_name = "MODEL[+EFFORT]", requires = "print", value_parser = parse_model_override)]
    review_model: Option<(String, Option<String>)>,

    /// Override the default subagent model, or disable subagents, for this
    /// non-interactive invocation.
    ///
    /// Accepts an optional trailing `+<effort>` on the model, same as `--model`.
    #[arg(long, value_name = "MODEL[+EFFORT]|disabled|none", requires = "print", value_parser = parse_optional_role_override)]
    subagent_model: Option<(String, Option<String>)>,

    /// Output format for `--print`.
    #[arg(long, value_enum, default_value_t = HeadlessOutputFormat::Text)]
    output_format: HeadlessOutputFormat,

    /// Permission handling for `--print`.
    ///
    /// `manual` rejects permission prompts so headless runs never hang.
    /// `auto` accepts edit/delete/move prompts but rejects shell execution.
    /// `yolo` accepts every permission prompt.
    #[arg(long, value_enum)]
    permission_mode: Option<HeadlessPermissionMode>,

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

    /// Use this Anvil development binary instead of bundled or managed Anvil.
    #[arg(long, global = true, value_name = "PATH")]
    anvil_path: Option<PathBuf>,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Install repository guidance for coding agents.
    Agents(AgentsArgs),
    /// Inspect or refresh model discovery state.
    Models(ModelsArgs),
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

#[derive(Debug, clap::Args)]
struct AgentsArgs {
    #[command(subcommand)]
    command: AgentsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum AgentsCommand {
    /// Add Bifrost code-intelligence guidance to AGENTS.md.
    Install(AgentsInstallArgs),
}

#[derive(Debug, clap::Args)]
struct AgentsInstallArgs {
    /// Apply the displayed diff without an interactive confirmation.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, clap::Args)]
struct ModelsArgs {
    #[command(subcommand)]
    command: ModelsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ModelsCommand {
    /// Clear cached ACP capabilities so enabled adapters are probed again.
    Refresh,
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

/// Reasoning-effort tokens accepted as a trailing `+<effort>` suffix on a
/// role-override model selector, e.g. `custom/bpr-agent/...::model+high`.
/// Case-insensitive; `none` canonicalizes to `off` (matches Anvil's
/// `REASONING_EFFORT_OFF_VALUE`, which explicitly turns reasoning off
/// rather than leaving the adapter's default effort untouched).
const KNOWN_REASONING_EFFORTS: &[&str] = &[
    "off", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// Splits a trailing `+<effort>` suffix off a role-override selector.
///
/// Model wire ids from every current adapter (bedrock/deepseek/openai
/// selectors) never contain `+`, so a trailing `+<known-effort>` is
/// unambiguous: only the *last* `+`-delimited segment is considered, and
/// only when it matches a known effort token exactly (case-insensitively).
/// Anything else (including a selector with no `+` at all) is returned
/// unsplit with no effort.
fn split_role_effort(value: &str) -> (&str, Option<String>) {
    let Some(idx) = value.rfind('+') else {
        return (value, None);
    };
    let (model, suffix) = value.split_at(idx);
    let suffix = &suffix[1..];
    let lower = suffix.to_ascii_lowercase();
    if !KNOWN_REASONING_EFFORTS.contains(&lower.as_str()) {
        return (value, None);
    }
    let effort = if lower == "none" {
        "off".to_string()
    } else {
        lower
    };
    (model, Some(effort))
}

fn parse_model_override(value: &str) -> std::result::Result<(String, Option<String>), String> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => return Err("--model requires an explicit model, not 'auto'".to_string()),
        "disabled" | "none" => {
            return Err("the primary agent cannot be disabled".to_string());
        }
        _ => {}
    }
    if value.trim().is_empty() {
        return Err("--model requires a model".to_string());
    }
    let (model, effort) = split_role_effort(value);
    if model.trim().is_empty() {
        return Err("--model requires a model".to_string());
    }
    Ok((model.to_string(), effort))
}

fn parse_optional_role_override(
    value: &str,
) -> std::result::Result<(String, Option<String>), String> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => {
            return Err("role override requires an explicit model or 'disabled'".to_string());
        }
        "disabled" | "none" => return Ok((config::DISABLED_MODEL.to_string(), None)),
        _ => {}
    }
    if value.trim().is_empty() {
        return Err("role override requires a model".to_string());
    }
    let (model, effort) = split_role_effort(value);
    if model.trim().is_empty() {
        return Err("role override requires a model".to_string());
    }
    Ok((model.to_string(), effort))
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
    #[value(alias = "default")]
    Manual,
    #[value(alias = "acceptEdits", alias = "accept-edits")]
    Auto,
    #[value(alias = "bypassPermissions", alias = "bypass-permissions")]
    Yolo,
}

impl From<HeadlessPermissionMode> for headless::PermissionMode {
    fn from(value: HeadlessPermissionMode) -> Self {
        match value {
            HeadlessPermissionMode::Manual => Self::Manual,
            HeadlessPermissionMode::Auto => Self::Auto,
            HeadlessPermissionMode::Yolo => Self::Yolo,
        }
    }
}

impl From<HeadlessPermissionMode> for config::PermissionPreset {
    fn from(value: HeadlessPermissionMode) -> Self {
        match value {
            HeadlessPermissionMode::Manual => Self::Manual,
            HeadlessPermissionMode::Auto => Self::Auto,
            HeadlessPermissionMode::Yolo => Self::Yolo,
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
        Some(Commands::Agents(_)) => false,
        Some(Commands::Models(_)) => false,
        Some(Commands::Resume(args)) => !args.list,
        Some(Commands::Server(_)) => false,
        None => true,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    anvil::configure_cli_override(cli.anvil_path.clone());
    init_logging(cli.log_file.as_deref())?;
    let debug_file = cli.log_file.clone();
    let snapshot_exclusions =
        configured_snapshot_exclusions(cli.log_file.as_deref(), cli.agent_stderr.as_deref());
    let termination = termination::Coordinator::install();
    #[cfg(unix)]
    if std::env::var_os("MJ_TERMINATION_PTY_INTEGRATION").is_some() {
        return termination_pty_integration_helper(termination).await;
    }
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
            Commands::Agents(args) => match args.command {
                AgentsCommand::Install(args) => agent_instructions::install(&cwd, args.yes),
            },
            Commands::Models(args) => match args.command {
                ModelsCommand::Refresh => {
                    roster::invalidate_model_cache()?;
                    println!(
                        "Model cache cleared; the next model resolution will reprobe enabled ACP adapters."
                    );
                    Ok(())
                }
            },
            Commands::Resume(mut args) => {
                args.fullscreen_tui |= fullscreen_tui;
                run_resume(
                    args,
                    fs_max_text_bytes,
                    top_level_additional_directories,
                    debug_file,
                    cli.permission_mode.map(Into::into),
                    termination.token(),
                )
                .await
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
                    snapshot_exclusions,
                    fs_max_text_bytes,
                    termination: termination.token(),
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
            snapshot_exclusions,
            fs_max_text_bytes,
            output_format: cli.output_format.into(),
            permission_mode: cli
                .permission_mode
                .unwrap_or(HeadlessPermissionMode::Manual)
                .into(),
            permission_config_mode: cli.permission_mode.map(Into::into),
            role_overrides: config::ModelOverrides {
                primary: cli.model.as_ref().map(|(model, _)| model.clone()),
                primary_effort: cli.model.and_then(|(_, effort)| effort),
                review: cli.review_model.as_ref().map(|(model, _)| model.clone()),
                review_effort: cli.review_model.and_then(|(_, effort)| effort),
                subagent: cli.subagent_model.as_ref().map(|(model, _)| model.clone()),
                subagent_effort: cli.subagent_model.and_then(|(_, effort)| effort),
            },
            termination: termination.token(),
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
            snapshot_exclusions,
            additional_directories: workspace_roots.additional_directories().to_vec(),
            fs_max_text_bytes,
            permission_mode: cli.permission_mode.map(Into::into),
            termination: termination.token(),
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

/// Minimal real-binary path used only by the Unix PTY termination integration
/// test. It deliberately waits on the installed coordinator so the test covers
/// the operating system signal listener rather than a test-only cancellation
/// path. The `force` mode keeps terminal ownership after acknowledging the
/// first signal, allowing the integration test to deliver a real second signal.
#[cfg(unix)]
async fn termination_pty_integration_helper(termination: termination::Coordinator) -> Result<()> {
    let _terminal = FullscreenTerminal::fresh().context("setup termination PTY terminal")?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format!("MJ_TERMINATION_PTY_READY:{}\n", std::process::id()).as_bytes())
        .context("write termination PTY readiness marker")?;
    stdout
        .flush()
        .context("flush termination PTY readiness marker")?;
    termination.token().cancelled().await;
    if std::env::var_os("MJ_TERMINATION_PTY_INTEGRATION").is_some_and(|mode| mode == "force") {
        stdout
            .write_all(b"MJ_TERMINATION_PTY_FIRST_SIGNAL_ACK\n")
            .context("write termination PTY first-signal acknowledgement")?;
        stdout
            .flush()
            .context("flush termination PTY first-signal acknowledgement")?;
        std::future::pending::<()>().await;
    }
    Ok(())
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

fn primary_session_routes(roster: &roster::Roster) -> Vec<roster::ResolvedAgent> {
    let mut routes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for role in std::iter::once(&roster.primary).chain(roster.available.iter()) {
        if role.ranked && seen.insert(role.launch.source_id.clone()) {
            routes.push(role.clone());
        }
    }
    routes
}

fn models_reload_message(roster: &roster::Roster) -> String {
    let role = |role: Option<&roster::ResolvedAgent>| {
        role.map(|role| format!("{} via {}", role.model.model, role.launch.source_id))
            .unwrap_or_else(|| "off".to_string())
    };
    format!(
        "Models reloaded after /clear: primary {}; subagents {}",
        role(Some(&roster.primary)),
        role(roster.subagent_default.as_ref()),
    )
}

async fn list_agent_sessions(
    roster: &roster::Roster,
    cwd: &Path,
    agent_stderr: Option<&Path>,
) -> Vec<session::SessionEntry> {
    let mut sessions = Vec::new();
    for role in primary_session_routes(roster) {
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
                "list agent sessions: {error:#}"
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
    roster: &'a roster::Roster,
    entry: &session::SessionEntry,
) -> Option<&'a roster::ResolvedAgent> {
    let adapter = entry.adapter_source_id.as_deref()?;
    entry
        .model
        .as_deref()
        .and_then(|model| {
            roster
                .available
                .iter()
                .find(|role| role.launch.source_id == adapter && role.model.model == model)
        })
        .or_else(|| {
            roster
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
    debug_file: Option<PathBuf>,
    permission_mode: Option<config::PermissionPreset>,
    termination: CancellationToken,
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
    let mut resume_roster = if args.list {
        roster::resolve(&cfg, &cwd).await?
    } else {
        resolve_roster_for_tui(&cfg, &cwd, false).await?
    };
    let mut agent = selected_agent_for_role(&resume_roster.primary);
    if let Some(session_id) = args.session_id.as_deref()
        && let Some(record) = session_provenance::find(session_id, &cwd)
    {
        let pinned = resume_roster
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
            })?
            .clone();
        resume_roster.primary = pinned.clone();
        resume_roster.rebind_auto_review_for_primary(&cfg);
        agent = selected_agent_for_role(&pinned);
    } else if let Some(session_id) = args.session_id.as_deref() {
        let matches = list_agent_sessions(&resume_roster, &cwd, args.agent_stderr.as_deref())
            .await
            .into_iter()
            .filter(|entry| entry.session_id == session_id)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [entry] => {
                let role = role_for_session_entry(&resume_roster, entry)
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
                resume_roster.primary = role;
                resume_roster.rebind_auto_review_for_primary(&cfg);
            }
            [] => {}
            _ => anyhow::bail!(
                "legacy session ID {session_id} is ambiguous across ACP adapters; select it with `mj resume` first"
            ),
        }
    }

    // `--list`: headless listing, print and exit.
    if args.list {
        let sessions =
            list_agent_sessions(&resume_roster, &cwd, args.agent_stderr.as_deref()).await;
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
                snapshot_exclusions: configured_snapshot_exclusions(
                    debug_file.as_deref(),
                    args.agent_stderr.as_deref(),
                ),
                additional_directories: additional_directories.clone(),
                fs_max_text_bytes,
                permission_mode,
                termination: termination.clone(),
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
            list_agent_sessions(&resume_roster, &cwd, args.agent_stderr.as_deref()).await;
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
            termination.clone(),
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
                    match role_for_session_entry(&resume_roster, &entry) {
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
                let role = role_for_session_entry(&resume_roster, &entry)
                    .ok_or_else(|| anyhow::anyhow!("selected session route is unavailable"))?
                    .clone();
                agent = selected_agent_for_role(&role);
                resume_roster.primary = role;
                resume_roster.rebind_auto_review_for_primary(&cfg);
                let result = run_app(
                    cwd,
                    RuntimeOptions {
                        snapshot_exclusions: configured_snapshot_exclusions(
                            debug_file.as_deref(),
                            args.agent_stderr.as_deref(),
                        ),
                        agent_stderr: args.agent_stderr,
                        additional_directories: additional_directories.clone(),
                        fs_max_text_bytes,
                        permission_mode,
                        termination: termination.clone(),
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

fn configured_snapshot_exclusions(
    debug_file: Option<&Path>,
    agent_stderr: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = debug_file
        .into_iter()
        .chain(agent_stderr)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
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
    snapshot_exclusions: Vec<PathBuf>,
    additional_directories: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    permission_mode: Option<config::PermissionPreset>,
    termination: CancellationToken,
}

struct RunSessionResult {
    reason: UiExitReason,
    session_id: Option<String>,
    session_title: Option<String>,
    theme_kind: theme::TerminalThemeKind,
    spinner_style: spinner::SpinnerStyle,
}

async fn start_new_session_loading() -> Option<(CancellationToken, tokio::task::JoinHandle<()>)> {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return None;
    }
    if write!(stdout, "\r\x1b[2Kloading.")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return None;
    }
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let mut dots = 2;
        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(350)) => {}
            }
            if write!(stdout, "\r\x1b[2Kloading{}", ".".repeat(dots))
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return;
            }
            dots = dots % 3 + 1;
        }
    });
    Some((cancel, task))
}

async fn stop_new_session_loading(
    loading: Option<(CancellationToken, tokio::task::JoinHandle<()>)>,
) {
    let Some((cancel, task)) = loading else {
        return;
    };
    cancel.cancel();
    let _ = task.await;
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\r\x1b[2K").and_then(|()| stdout.flush());
}

struct ActiveSideRuntime {
    session_id: String,
    commands: mpsc::UnboundedSender<UiCommand>,
    runtime_task: tokio::task::JoinHandle<()>,
    event_task: tokio::task::JoinHandle<()>,
}

fn isolated_side_runtime_config(
    agent: &SelectedAgent,
    resume_session: Option<String>,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    agent_stderr: Option<PathBuf>,
    fs_max_text_bytes: u64,
) -> acp::AcpRuntimeConfig {
    acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd,
        additional_directories,
        mcp_servers: Vec::new(),
        resume_session,
        session_restore_mode: acp::SessionRestoreMode::Continue,
        env: agent.env.clone(),
        agent_stderr,
        fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: None,
        config_path: None,
        saved_session_config: std::collections::HashMap::new(),
        role_config: None,
        subagents: None,
        side_prompt_policy: true,
        termination: None,
    }
}

async fn discard_side_runtime(
    side: ActiveSideRuntime,
    agent: &SelectedAgent,
    agent_stderr: Option<&Path>,
) -> Option<String> {
    let _ = side.commands.send(UiCommand::CancelPrompt);
    let _ = side.commands.send(UiCommand::Shutdown);
    let mut runtime_task = side.runtime_task;
    if tokio::time::timeout(Duration::from_secs(2), &mut runtime_task)
        .await
        .is_err()
    {
        runtime_task.abort();
        let _ = runtime_task.await;
    }
    side.event_task.abort();
    match tokio::time::timeout(
        Duration::from_secs(5),
        session::delete_session(agent, side.session_id.clone(), agent_stderr),
    )
    .await
    {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!(
            "could not delete side session {}: {error:#}",
            side.session_id
        )),
        Err(_) => Some(format!(
            "timed out deleting side session {}",
            side.session_id
        )),
    }
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

async fn resolve_roster_for_tui(
    cfg: &Config,
    cwd: &Path,
    wait_for_installs: bool,
) -> Result<roster::Roster> {
    with_startup_spinner(async {
        if wait_for_installs {
            roster::resolve_waiting_for_installs(cfg, cwd).await
        } else {
            roster::resolve(cfg, cwd).await
        }
    })
    .await
}

/// Resolve the roster for interactive startup without blocking on adapter
/// probes; the spinner only appears in the rare case where the instantly
/// known adapters cannot bind the configured roles.
async fn resolve_roster_streaming_for_tui(
    cfg: &Config,
    cwd: &Path,
) -> Result<roster::StreamingResolution> {
    with_startup_spinner(roster::resolve_streaming(cfg, cwd)).await
}

async fn with_startup_spinner<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let mut stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return future.await;
    }

    let mut resolution = Box::pin(future);
    let mut tick = tokio::time::interval(Duration::from_millis(125));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let started = Instant::now();
    let mut frame = 0_usize;
    let mut status_writable = true;
    loop {
        tokio::select! {
            result = &mut resolution => {
                if status_writable {
                    let _ = clear_startup_status(&mut stdout);
                }
                return result;
            }
            _ = tick.tick() => {
                if status_writable {
                    status_writable = write_startup_status(
                        &mut stdout,
                        frame,
                        started.elapsed(),
                    ).is_ok();
                }
                frame = frame.wrapping_add(1);
            }
        }
    }
}

fn write_startup_status(
    output: &mut impl Write,
    frame: usize,
    elapsed: Duration,
) -> std::io::Result<()> {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    write!(
        output,
        "\r\x1b[2K{} Discovering models... {}s",
        FRAMES[frame % FRAMES.len()],
        elapsed.as_secs()
    )?;
    output.flush()
}

fn clear_startup_status(output: &mut impl Write) -> std::io::Result<()> {
    output.write_all(b"\r\x1b[2K")?;
    output.flush()
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
    let termination = runtime_options.termination.clone();
    anvil::start_background_install();
    let config_path = config::default_config_path();
    let config_exists = config::Config::path_has_current_version(&config_path);
    let mut cfg = Config::load(&config_path)?;
    let onboarding_kind = onboarding_kind(
        config_exists,
        cfg.onboarding_version,
        resume_target.as_ref(),
        initial_agent.as_ref(),
    );
    if auth::detect(auth::AuthVendor::Kimi).available()
        && cfg.acp.policy("kimi") != config::AcpServerPolicy::Disabled
        && !cfg.acp.servers.iter().any(|server| server.id == "kimi")
    {
        kimi::start_background_install();
    }
    let mut roster_updates = None;
    let mut pending_probe_servers = Vec::new();
    let mut roster = if let Some(kind) = onboarding_kind {
        // Onboarding wants a fully settled catalog to preview, so first
        // startup and versioned education keep the blocking resolution.
        let initial_resolution = resolve_roster_for_tui(&cfg, &cwd, false).await;
        let Some((accepted_config, accepted_roster)) = run_startup_onboarding(
            kind,
            cfg,
            initial_resolution.ok(),
            &config_path,
            &cwd,
            termination.clone(),
        )
        .await?
        else {
            return Ok(None);
        };
        cfg = accepted_config;
        accepted_roster
    } else {
        let resolution = resolve_roster_streaming_for_tui(&cfg, &cwd).await?;
        roster_updates = resolution.updates;
        pending_probe_servers = resolution.pending_servers;
        resolution.roster
    };
    if let Some(agent) = initial_agent.as_ref()
        && let Some(pinned) = roster.available.iter().find(|role| {
            role.launch.command == agent.program
                && role.launch.args == agent.args
                && role.model.model == agent.source_id.trim_start_matches("roster:")
        })
    {
        roster.primary = pinned.clone();
        roster.rebind_auto_review_for_primary(&cfg);
    }
    let mut primary_agent = selected_agent_for_role(&roster.primary);

    // Consume resume_session and any pinned resume launch on the first
    // iteration only. Fresh sessions always use the resolved primary agent.
    let mut initial_resume = resume_target;
    let mut initial_agent = initial_agent.or_else(|| Some(primary_agent.clone()));
    let mut pending_new_session_boundary = false;
    let mut pending_models_boundary = None;
    loop {
        let resume = initial_resume.take();
        let agent = initial_agent
            .take()
            .unwrap_or_else(|| primary_agent.clone());

        let session_boundary = pending_models_boundary.take().or_else(|| {
            new_session_boundary_for_agent(
                std::mem::take(&mut pending_new_session_boundary),
                &agent,
            )
        });

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
            roster.clone(),
            cfg.agent.clone(),
            cfg.subagents.clone(),
            roster_updates.take(),
            std::mem::take(&mut pending_probe_servers),
            termination.clone(),
        )
        .await?;
        apply_session_result_to_config(&mut cfg, &session_result);
        match session_result.reason {
            UiExitReason::Quit => return Ok(session_result.session_id),
            UiExitReason::NewSession | UiExitReason::ClearSession => {
                let show_new_session_boundary = session_result.reason == UiExitReason::NewSession;
                cfg = Config::load(&config_path)?;
                let resolution = resolve_roster_streaming_for_tui(&cfg, &cwd).await?;
                roster = resolution.roster;
                roster_updates = resolution.updates;
                pending_probe_servers = resolution.pending_servers;
                primary_agent = selected_agent_for_role(&roster.primary);
                initial_agent = Some(primary_agent.clone());
                pending_new_session_boundary = show_new_session_boundary;
                if session_result.reason == UiExitReason::ClearSession {
                    pending_models_boundary = Some(models_reload_message(&roster));
                }
                continue;
            }
            UiExitReason::SwitchSession => {
                if let Some(session_id) = session_result.session_id {
                    let resume_agent = session_provenance::find(&session_id, &cwd)
                        .and_then(|record| {
                            roster.available.iter().find(|role| {
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
                    termination.clone(),
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

fn onboarding_kind(
    config_exists: bool,
    onboarding_version: u32,
    resume_target: Option<&ResumeTarget>,
    initial_agent: Option<&SelectedAgent>,
) -> Option<onboarding::Kind> {
    if resume_target.is_some() || initial_agent.is_some() {
        return None;
    }
    if !config_exists {
        return Some(onboarding::Kind::Fresh);
    }
    (onboarding_version < config::ONBOARDING_CONTENT_VERSION).then_some(onboarding::Kind::Upgrade)
}

async fn run_startup_onboarding(
    kind: onboarding::Kind,
    candidate: Config,
    preview: Option<roster::Roster>,
    config_path: &Path,
    cwd: &Path,
    termination: CancellationToken,
) -> Result<Option<(Config, roster::Roster)>> {
    let outcome = run_onboarding_once(kind, candidate, preview, None, cwd, termination).await?;
    match outcome {
        onboarding::Outcome::Accept(next, resolved) => {
            let next = *next;
            next.save(config_path)
                .with_context(|| format!("save {}", config_path.display()))?;
            Ok(Some((next, *resolved)))
        }
        onboarding::Outcome::Skip(next) => {
            let next = *next;
            next.save(config_path)
                .with_context(|| format!("save {}", config_path.display()))?;
            let resolved = resolve_roster_for_tui(&next, cwd, false).await?;
            Ok(Some((next, resolved)))
        }
        onboarding::Outcome::Cancel => Ok(None),
    }
}

async fn run_onboarding_once(
    kind: onboarding::Kind,
    config: Config,
    roster: Option<roster::Roster>,
    notice: Option<String>,
    cwd: &Path,
    termination: CancellationToken,
) -> Result<onboarding::Outcome> {
    let mut terminal = FullscreenTerminal::fresh().context("setup onboarding terminal")?;
    let outcome = onboarding::run(
        terminal.terminal_mut(),
        kind,
        config,
        roster,
        notice,
        cwd,
        termination,
    )
    .await;
    terminal.restore_once();
    settle_after_fullscreen_picker_restore().await;
    outcome
}

async fn run_session_picker_action_for_agent(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
    current_session_id: Option<String>,
    current_session_title: Option<String>,
    theme: palette::TerminalTheme,
    termination: CancellationToken,
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
        let outcome = run_session_picker_once(
            listing.sessions,
            delete_supported,
            notice.take(),
            theme,
            termination.clone(),
        )
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

async fn run_session_picker_action_for_roster(
    roster: &roster::Roster,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
    current_session_id: Option<String>,
    current_session_title: Option<String>,
    theme: palette::TerminalTheme,
    termination: CancellationToken,
) -> Result<(SessionPickerAction, Option<roster::ResolvedAgent>)> {
    let mut notice = None;
    loop {
        let sessions = list_agent_sessions(roster, &cwd, agent_stderr).await;
        if sessions.is_empty() {
            return Ok((
                session_picker_empty_action(current_session_id, current_session_title),
                None,
            ));
        }
        let outcome =
            run_session_picker_once(sessions, true, notice.take(), theme, termination.clone())
                .await?;
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
                notice = match role_for_session_entry(roster, &entry) {
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
                let role = role_for_session_entry(roster, &entry)
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
    termination: CancellationToken,
) -> Result<session::ResumeOutcome> {
    let mut terminal = FullscreenTerminal::fresh().context("setup terminal")?;
    let outcome = session::run_session_picker(
        terminal.terminal_mut(),
        sessions,
        delete_supported,
        notice,
        theme,
        termination,
    )
    .await;
    terminal.restore_once();
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

fn selected_agent_for_role(role: &roster::ResolvedAgent) -> SelectedAgent {
    SelectedAgent {
        source_id: format!("roster:{}", role.model.model),
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
    roster: roster::Roster,
    agent_config: config::AgentConfig,
    subagents_config: config::SubagentsConfig,
    roster_updates: Option<tokio::sync::watch::Receiver<roster::Roster>>,
    pending_probe_servers: Vec<String>,
    termination: CancellationToken,
) -> Result<RunSessionResult> {
    let mut terminal = SessionTerminal::fresh(mode)?;
    let session_tag = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let (subagent_roles, _subagent_codex_home) =
        isolated_subagent_roles(roster.subagent_failover_roles(), "subagent")?;

    let (event_tx, runtime_event_rx) = mpsc::unbounded_channel();
    let (ui_event_tx, ui_event_rx) = mpsc::unbounded_channel();
    let (runtime_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel();
    let quota_gate = quota::Gate::new(cwd.clone(), ui_event_tx.clone());
    let subagent_pool = (!subagent_roles.is_empty()).then(|| {
        quota::RolePool::new(
            subagent_roles.clone(),
            quota_gate.clone(),
            subagents_config.auto_failover,
            "subagents",
            ui_event_tx.clone(),
        )
    });
    let subagent_handoffs_this_turn = Arc::new(AtomicUsize::new(0));
    // One id sequence for pool subagents and review lanes alike: both render as
    // rows in the same status area.
    let subagent_ids = subagent::SubagentIdAllocator::default();
    let active_implementation_workers = subagent::ActiveSubagentWorkers::default();
    let (subagent_reports, subagent_report_rx) = subagent::SubagentReportBus::channel();
    // Shared with the orchestrator so every wake can ask the still-running
    // subagents for progress.
    let subagent_runs = subagent::SubagentRegistry::default();
    tracing::info!(
        event = "roster_setup",
        session_tag = %session_tag,
        seat = "primary",
        model = %roster.primary.model.model,
        model_value = %roster.primary.model_value,
        adapter = %roster.primary.launch.source_id,
        "seat configured"
    );
    if let Some(role) = roster.subagent_default.as_ref() {
        tracing::info!(
            event = "roster_setup",
            session_tag = %session_tag,
            seat = "subagents",
            model = %role.model.model,
            model_value = %role.model_value,
            adapter = %role.launch.source_id,
            "seat configured"
        );
    } else {
        tracing::info!(
            event = "roster_setup",
            session_tag = %session_tag,
            seat = "subagents",
            model = "disabled",
            "seat disabled"
        );
    }
    let _ = ui_event_tx.send(crate::event::UiEvent::Info(format!(
        "Agents · primary {} · subagents {} · {} launchable models",
        roster.primary.model.model,
        roster
            .subagent_default
            .as_ref()
            .map(|role| role.model.model.as_str())
            .unwrap_or("off"),
        roster.available.len(),
    )));
    for warning in &roster.warnings {
        let _ = ui_event_tx.send(crate::event::UiEvent::Warning(warning.clone()));
    }
    let _ = pending_probe_servers;
    let roster_update_task = roster_updates.map(|mut updates| {
        let tx = ui_event_tx.clone();
        let mut surfaced: std::collections::HashSet<String> =
            roster.warnings.iter().cloned().collect();
        tokio::spawn(async move {
            while updates.changed().await.is_ok() {
                let snapshot = updates.borrow_and_update().clone();
                for warning in &snapshot.warnings {
                    if surfaced.insert(warning.clone())
                        && tx
                            .send(crate::event::UiEvent::Warning(warning.clone()))
                            .is_err()
                    {
                        return;
                    }
                }
                if tx
                    .send(crate::event::UiEvent::RosterUpdate {
                        choices: snapshot.choices,
                        inventory: snapshot.inventory,
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    });
    let usage_roles = std::iter::once(&roster.primary).chain(subagent_roles.iter());
    let mut claude_usage_env = None;
    let mut codex_usage_env = None;
    for role in usage_roles {
        match role.launch.source_id.as_str() {
            "claude-acp" if claude_usage_env.is_none() => {
                claude_usage_env = Some(role.launch.env.clone());
            }
            "codex-acp" if codex_usage_env.is_none() => {
                codex_usage_env = Some(role.launch.env.clone());
            }
            _ => {}
        }
    }
    let has_usage_poller = claude_usage_env.is_some() || codex_usage_env.is_some();
    let (usage_turn_tx, usage_shutdown_tx, usage_task) = if has_usage_poller {
        let (tx, mut rx) = mpsc::unbounded_channel::<UsageRefreshTrigger>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
        let usage_ui_tx = ui_event_tx.clone();
        let usage_cwd = cwd.clone();
        let _ = tx.send(UsageRefreshTrigger::Startup);
        let handle = tokio::spawn(async move {
            let mut completed_turns = 0_u64;
            let mut codex_client = None;
            loop {
                let trigger = tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => break,
                    trigger = rx.recv() => {
                        let Some(trigger) = trigger else { break; };
                        if matches!(trigger, UsageRefreshTrigger::CompletedTurn) {
                            completed_turns = completed_turns.saturating_add(1);
                        }
                        trigger
                    },
                };
                if let Some(env) = codex_usage_env.as_ref() {
                    let status =
                        codex_usage::refresh(&mut codex_client, usage_cwd.clone(), env.clone())
                            .await;
                    if usage_ui_tx
                        .send(crate::event::UiEvent::CodexUsage(status))
                        .is_err()
                    {
                        break;
                    }
                }
                if should_refresh_claude_usage(trigger, completed_turns)
                    && let Some(env) = claude_usage_env.as_ref()
                {
                    let status = match claude_usage::query(usage_cwd.clone(), env.clone()).await {
                        Ok(report) => claude_usage::ClaudeUsageStatus::Available(report),
                        Err(error) => {
                            tracing::warn!("claude /usage failed: {error}");
                            claude_usage::ClaudeUsageStatus::Unavailable(
                                error.user_reason().to_string(),
                            )
                        }
                    };
                    if usage_ui_tx
                        .send(crate::event::UiEvent::ClaudeUsage(status))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            if let Some(client) = codex_client {
                client.shutdown().await;
            }
        });
        (Some(tx), Some(shutdown_tx), Some(handle))
    } else {
        (None, None, None)
    };
    let mut ui_event_rx = ui_event_rx;

    // The discrete review's specialist lanes run on the subagent seat, so they
    // need the pool that is about to move into the subagent config.
    let review_workers = subagent_pool.clone();

    let mut primary_env = agent.env.clone();
    let primary_permission = runtime_options.permission_mode.and_then(|mode| {
        roster::configure_permissions(roster.primary.launch.kind, mode, &mut primary_env)
    });
    let runtime_cfg = acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd: cwd.clone(),
        additional_directories: runtime_options.additional_directories.clone(),
        mcp_servers: Vec::new(),
        resume_session,
        session_restore_mode: acp::SessionRestoreMode::Replay,
        env: primary_env,
        agent_stderr: runtime_options.agent_stderr.clone(),
        fs_max_text_bytes: runtime_options.fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: Some(roster.primary.launch.source_id.clone()),
        config_path: Some(config::default_config_path()),
        saved_session_config: config::load_saved_session_config(
            &config::default_config_path(),
            &roster.primary.launch.source_id,
            &roster.primary.model.model,
            config::SessionConfigSeat::Primary,
        ),
        role_config: Some(acp::RuntimeRoleConfig {
            label: "primary".to_string(),
            model_id: roster.primary.model.model.clone(),
            model_value: roster.primary.model_value.clone(),
            adapter_source_id: roster.primary.launch.source_id.clone(),
            require_native_read_only: false,
            permission: primary_permission,
            session_tag: Some(session_tag.clone()),
            reasoning_effort: roster.primary.reasoning_effort.clone(),
        }),
        subagents: subagent_pool.map(|subagent_pool| {
            let mut config =
                subagent::Config::new(subagent_pool, runtime_options.agent_stderr.clone());
            if let Some(role) = config.role_config.as_mut() {
                role.session_tag = Some(session_tag.clone());
            }
            config
                .with_subagent_handoff_counter(subagent_handoffs_this_turn.clone())
                .with_id_allocator(subagent_ids.clone())
                .with_active_implementation_workers(active_implementation_workers.clone())
                .with_max_parallel(subagents_config.max_parallel)
                .with_debrief(subagents_config.debrief)
                .with_reports(subagent_reports.clone())
                .with_run_registry(subagent_runs.clone())
                .with_prewarm(subagent::RunContext {
                    cwd: cwd.clone(),
                    additional_directories: runtime_options.additional_directories.clone(),
                    snapshot_exclusions: runtime_options.snapshot_exclusions.clone(),
                    fs_max_text_bytes: runtime_options.fs_max_text_bytes,
                    access_mode: acp::RuntimeAccessMode::Full,
                })
        }),
        side_prompt_policy: false,
        termination: None,
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
    let agent_display_name = Some(format!(
        "{} via {}",
        roster.primary.model.model, roster.primary.launch.source_id
    ));
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
        roster.primary.model.model.clone(),
        remote::TrackerStatusSeed {
            model_source: Some(roster.primary.launch.source_id.clone()),
            reasoning_effort: roster.primary.reasoning_effort.clone(),
            cwd: Some(cwd.clone()),
        },
        Some(cmd_tx.clone()),
        Some(ui_event_tx.clone()),
    );
    let orchestrated = orchestrator::spawn(
        runtime_event_rx,
        orchestrator::Config {
            runtime_commands: runtime_cmd_tx.clone(),
            active_subagent_workers: active_implementation_workers.clone(),
            subagent_reports: subagent_report_rx,
            subagent_report_bus: subagent_reports.clone(),
            subagent_runs,
            progress_wake: orchestrator::progress_wake_interval(
                subagents_config.progress_wake_minutes,
            ),
            discrete_review: agent_config.discrete_review,
            max_correction_rounds: agent_config.max_correction_rounds,
            primary_model: Some(roster.primary.model.model.clone()),
            review_root: cwd.clone(),
            review_fanout: review_workers.zip(roster.review_supervisor.clone()).map(
                |(workers, supervisor)| {
                    discrete_review::Spawner::live(discrete_review::FanoutConfig {
                        workers,
                        supervisor,
                        cwd: cwd.clone(),
                        additional_directories: runtime_options.additional_directories.clone(),
                        session_tag: Some(session_tag.clone()),
                        agent_stderr: runtime_options.agent_stderr.clone(),
                        snapshot_exclusions: runtime_options.snapshot_exclusions.clone(),
                        fs_max_text_bytes: runtime_options.fs_max_text_bytes,
                        id_allocator: subagent_ids.clone(),
                    })
                },
            ),
        },
    );
    let primary_orchestrator = orchestrated.handle.clone();
    let refresh_usage_on_failure = roster.primary.launch.source_id == "codex-acp";
    let event_usage_turn_tx = usage_turn_tx.clone();
    let event_tracker = remote_tracker.clone();
    let event_primary = roster.primary.clone();
    let event_cwd = cwd.clone();
    let side_ui_event_tx = ui_event_tx.clone();
    let event_proxy = tokio::spawn(async move {
        let mut events = orchestrated.events;
        while let Some(event) = events.recv().await {
            if let UiEvent::SessionStarted { session_id, .. } = &event {
                session_provenance::record(session_provenance::Record {
                    session_id: session_id.clone(),
                    cwd: event_cwd.clone(),
                    adapter_source_id: event_primary.launch.source_id.clone(),
                    model: event_primary.model.model.clone(),
                    model_value: event_primary.model_value.clone(),
                });
            }
            let event = event_tracker.intercept_event(event);
            if refresh_usage_on_failure
                && matches!(event, UiEvent::PromptFailed { .. })
                && let Some(tx) = event_usage_turn_tx.as_ref()
            {
                let _ = tx.send(UsageRefreshTrigger::CodexOnly);
            }
            let completed = matches!(event, UiEvent::PromptDone { .. });
            event_tracker.observe_event(&event);
            if ui_event_tx.send(event).is_err() {
                break;
            }
            if completed && let Some(tx) = event_usage_turn_tx.as_ref() {
                let _ = tx.send(UsageRefreshTrigger::CompletedTurn);
            }
        }
        let _ = orchestrated.task.await;
    });

    let cmd_tracker = remote_tracker.clone();
    let cmd_orchestrator = primary_orchestrator.clone();
    let mut cmd_workspace_roots =
        Vec::with_capacity(1 + runtime_options.additional_directories.len());
    cmd_workspace_roots.push(cwd.clone());
    cmd_workspace_roots.extend(runtime_options.additional_directories.iter().cloned());
    let cmd_snapshot_exclusions = runtime_options.snapshot_exclusions.clone();
    let side_agent = agent.clone();
    let side_cwd = cwd.clone();
    let side_additional_directories = runtime_options.additional_directories.clone();
    let side_agent_stderr = runtime_options.agent_stderr.clone();
    let side_fs_max_text_bytes = runtime_options.fs_max_text_bytes;
    let cmd_proxy = tokio::spawn(async move {
        let mut side_runtime: Option<ActiveSideRuntime> = None;
        let mut local_epoch = 0_u64;
        while let Some(command) = ui_cmd_rx.recv().await {
            if matches!(&command, UiCommand::StartSide) {
                if side_runtime.is_some() {
                    let _ = side_ui_event_tx.send(UiEvent::SideStartFailed {
                        message: "a side conversation is already active".to_string(),
                    });
                    continue;
                }
                let (responder, response) = tokio::sync::oneshot::channel();
                if runtime_cmd_tx
                    .send(UiCommand::ForkSideSession { responder })
                    .is_err()
                {
                    let _ = side_ui_event_tx.send(UiEvent::SideStartFailed {
                        message: "the main ACP runtime closed before side startup".to_string(),
                    });
                    continue;
                }
                let source = match response.await {
                    Ok(Ok(source)) => source,
                    Ok(Err(message)) => {
                        let _ = side_ui_event_tx.send(UiEvent::SideStartFailed { message });
                        continue;
                    }
                    Err(_) => {
                        let _ = side_ui_event_tx.send(UiEvent::SideStartFailed {
                            message: "the main ACP runtime dropped the side fork response"
                                .to_string(),
                        });
                        continue;
                    }
                };
                let fork_source = source.has_history;
                let resume_session = fork_source.then_some(source.session_id);

                let (side_event_tx, mut side_event_rx) = mpsc::unbounded_channel();
                let (side_cmd_tx, side_cmd_rx) = mpsc::unbounded_channel();
                let side_cfg = isolated_side_runtime_config(
                    &side_agent,
                    resume_session,
                    side_cwd.clone(),
                    side_additional_directories.clone(),
                    side_agent_stderr.clone(),
                    side_fs_max_text_bytes,
                );
                let runtime_task = tokio::spawn(async move {
                    let _ = acp::run(side_cfg, side_event_tx, side_cmd_rx).await;
                });
                let forwarded_events = side_ui_event_tx.clone();
                let (child_ready_tx, child_ready_rx) = tokio::sync::oneshot::channel();
                let expected_session_starts = if fork_source { 2 } else { 1 };
                let event_task = tokio::spawn(async move {
                    let mut child_ready_tx = Some(child_ready_tx);
                    let mut session_starts = 0_u8;
                    let mut started = false;
                    while let Some(event) = side_event_rx.recv().await {
                        if let UiEvent::SessionStarted { session_id, .. } = &event {
                            session_starts = session_starts.saturating_add(1);
                            if session_starts < expected_session_starts {
                                continue;
                            }
                            if session_starts == expected_session_starts {
                                started = true;
                                if let Some(tx) = child_ready_tx.take() {
                                    let _ = tx.send(Ok(session_id.clone()));
                                }
                            }
                        } else if !started {
                            let failure = match &event {
                                UiEvent::SessionForkFailed { message }
                                | UiEvent::Fatal(message) => Some(message.clone()),
                                _ => None,
                            };
                            if let Some(message) = failure {
                                if let Some(tx) = child_ready_tx.take() {
                                    let _ = tx.send(Err(message));
                                }
                                break;
                            }
                            continue;
                        }
                        if forwarded_events
                            .send(UiEvent::Side(Box::new(event)))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                if fork_source && side_cmd_tx.send(UiCommand::ForkSession).is_err() {
                    runtime_task.abort();
                    event_task.abort();
                    let _ = side_ui_event_tx.send(UiEvent::SideStartFailed {
                        message: "the side ACP runtime closed before forking".to_string(),
                    });
                    continue;
                }
                let child_session_id =
                    match tokio::time::timeout(Duration::from_secs(15), child_ready_rx).await {
                        Ok(Ok(Ok(session_id))) => session_id,
                        Ok(Ok(Err(message))) => {
                            let _ = side_cmd_tx.send(UiCommand::Shutdown);
                            event_task.abort();
                            let _ = side_ui_event_tx.send(UiEvent::SideStartFailed { message });
                            continue;
                        }
                        Ok(Err(_)) => {
                            let _ = side_cmd_tx.send(UiCommand::Shutdown);
                            event_task.abort();
                            let _ = side_ui_event_tx.send(UiEvent::SideStartFailed {
                                message: "the side ACP runtime dropped its fork result".to_string(),
                            });
                            continue;
                        }
                        Err(_) => {
                            let _ = side_cmd_tx.send(UiCommand::Shutdown);
                            event_task.abort();
                            let _ = side_ui_event_tx.send(UiEvent::SideStartFailed {
                                message: "side session fork timed out".to_string(),
                            });
                            continue;
                        }
                    };
                side_runtime = Some(ActiveSideRuntime {
                    session_id: child_session_id,
                    commands: side_cmd_tx,
                    runtime_task,
                    event_task,
                });
                continue;
            }
            if matches!(command, UiCommand::ExitSide) {
                if let Some(side) = side_runtime.take()
                    && let Some(message) =
                        discard_side_runtime(side, &side_agent, side_agent_stderr.as_deref()).await
                {
                    let _ = side_ui_event_tx.send(UiEvent::Warning(message));
                }
                continue;
            }
            let (command, force_main) = match command {
                UiCommand::Main(command) => (*command, true),
                command => (command, false),
            };
            if !force_main && side_runtime.is_some() {
                if matches!(command, UiCommand::Shutdown) {
                    if let Some(side) = side_runtime.take() {
                        let _ =
                            discard_side_runtime(side, &side_agent, side_agent_stderr.as_deref())
                                .await;
                    }
                } else {
                    let side = side_runtime.as_ref().expect("checked side runtime");
                    let _ = side.commands.send(command);
                    continue;
                }
            }
            cmd_tracker.observe_command(&command);
            if let UiCommand::SetReviewPolicy { enabled } = &command {
                cmd_orchestrator.set_review_enabled(*enabled);
                continue;
            }
            if let UiCommand::RunReview { target } = command {
                cmd_orchestrator.request_review(target);
                continue;
            }
            if matches!(command, UiCommand::CompactPrimary) {
                cmd_orchestrator.compact_manual().await;
                continue;
            }
            if let UiCommand::SendPrompt { text, images } = &command {
                local_epoch = local_epoch.saturating_add(1);
                subagent_handoffs_this_turn.store(0, Ordering::Release);
                let snapshot = workspace_snapshot::WorkspaceSnapshot::capture_excluding(
                    &cmd_workspace_roots,
                    &cmd_snapshot_exclusions,
                )
                .await;
                cmd_orchestrator
                    .begin_turn(local_epoch, text.clone(), images.clone(), snapshot)
                    .await;
            }
            if matches!(command, UiCommand::CancelPrompt) {
                cmd_orchestrator.cancel_review();
            }
            let shutdown = matches!(command, UiCommand::Shutdown);
            if runtime_cmd_tx.send(command).is_err() || shutdown {
                break;
            }
        }
        if let Some(side) = side_runtime.take()
            && let Some(message) =
                discard_side_runtime(side, &side_agent, side_agent_stderr.as_deref()).await
        {
            let _ = side_ui_event_tx.send(UiEvent::Warning(message));
        }
    });

    let mut header_labels = header_labels;
    let ui_result = loop {
        let ui_result = ui::run(
            terminal.terminal_mut(),
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
                feature_hints_enabled: config::Config::load(&config_path)
                    .map(|config| config.feature_hints)
                    .unwrap_or(true),
                active_agent_launch: Some(ragnarok::Launch {
                    program: agent.program.clone(),
                    args: agent.args.clone(),
                    env: agent.env.clone(),
                }),
                session_boundary: session_boundary.take(),
                session_cwd: cwd.clone(),
                model_choices: roster.choices.clone(),
                acp_inventory: roster.inventory.clone(),
                configured_models: config::Config::load(&config_path)
                    .map(|config| config.model_names())
                    .unwrap_or_default(),
                active_models: config::ModelsConfig {
                    primary: roster.primary.model.model.clone(),
                    primary_source: Some(roster.primary.launch.source_id.clone()),
                    review: roster
                        .review_supervisor
                        .as_ref()
                        .map(|role| role.model.model.clone())
                        .unwrap_or_else(|| "off".to_string()),
                    review_source: roster
                        .review_supervisor
                        .as_ref()
                        .map(|role| role.launch.source_id.clone()),
                    subagent: roster
                        .subagent_default
                        .as_ref()
                        .map(|role| role.model.model.clone())
                        .unwrap_or_else(|| "off".to_string()),
                    subagent_source: roster
                        .subagent_default
                        .as_ref()
                        .map(|role| role.launch.source_id.clone()),
                },
                review_enabled: agent_config.discrete_review,
                ragnarok_models: roster.available.clone(),
                primary_acp_name: roster.primary.launch.kind.display_name().to_string(),
                primary_reasoning_effort: roster.primary.reasoning_effort.clone(),
                termination: termination.clone(),
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
        terminal.restore_once();

        let current_session_id = result.session_id;
        let current_session_title = result.session_title;

        let (action, selected_role) = match run_session_picker_action_for_roster(
            &roster,
            cwd.clone(),
            runtime_options.agent_stderr.as_deref(),
            current_session_id.clone(),
            current_session_title.clone(),
            theme_kind.palette(),
            termination.clone(),
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
            role.launch.source_id != roster.primary.launch.source_id
                || role.model.model != roster.primary.model.model
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
                if roster.primary.launch.source_id == "codex-acp"
                    && let Some(tx) = usage_turn_tx.as_ref()
                {
                    let _ = tx.send(UsageRefreshTrigger::CodexOnly);
                }
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

    let new_session_loading = if matches!(
        ui_result.as_ref().map(|result| result.reason),
        Ok(UiExitReason::NewSession)
    ) {
        terminal.restore_once();
        start_new_session_loading().await
    } else {
        None
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

    if let Some(tx) = usage_shutdown_tx {
        let _ = tx.send(());
    }
    drop(usage_turn_tx);
    let event_proxy_wait = wait_for_task("remote-control event proxy", event_proxy);
    let cmd_proxy_wait = wait_for_task("remote-control command proxy", cmd_proxy);
    if let Some(task) = usage_task {
        tokio::join!(
            event_proxy_wait,
            cmd_proxy_wait,
            wait_for_task("subscription usage poller", task),
        );
    } else {
        tokio::join!(event_proxy_wait, cmd_proxy_wait);
    }
    if let Some(task) = roster_update_task {
        task.abort();
    }

    // Restore the terminal only now, after the runtime has finished tearing
    // down, so the session UI stays on screen through shutdown. `/new` restores
    // earlier to show its standalone loading line, and LoadSession restores
    // before showing the session picker; this is a no-op for both paths.
    terminal.restore_once();
    stop_new_session_loading(new_session_loading).await;
    if matches!(
        ui_result.as_ref().map(|result| result.reason),
        Ok(UiExitReason::ClearSession)
    ) && let Err(e) = ui::clear_terminal_screen(terminal.terminal_mut())
    {
        tracing::warn!("clear terminal for /clear failed: {e}");
    }

    ui_result
}

fn isolated_subagent_role(
    mut role: roster::ResolvedAgent,
    label: &str,
) -> Result<(roster::ResolvedAgent, Option<tempfile::TempDir>)> {
    if role.launch.kind != roster::AdapterKind::Codex {
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

fn isolated_subagent_roles(
    mut roles: Vec<roster::ResolvedAgent>,
    label: &str,
) -> Result<(Vec<roster::ResolvedAgent>, Option<tempfile::TempDir>)> {
    let Some(index) = roles
        .iter()
        .position(|role| role.launch.kind == roster::AdapterKind::Codex)
    else {
        return Ok((roles, None));
    };
    let (prepared, guard) = isolated_subagent_role(roles[index].clone(), label)?;
    let codex_home = prepared
        .launch
        .env
        .get("CODEX_HOME")
        .cloned()
        .expect("isolated Codex role has CODEX_HOME");
    roles[index] = prepared;
    for role in &mut roles {
        if role.launch.kind == roster::AdapterKind::Codex {
            role.launch
                .env
                .insert("CODEX_HOME".to_string(), codex_home.clone());
        }
    }
    Ok((roles, guard))
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

type Terminal = ratatui::Terminal<crate::term::TrackedBackend<std::io::Stdout>>;

/// A restoration operation owned alongside the terminal it cleans up.
///
/// The operation is deliberately invoked at most once, even when it fails:
/// retrying terminal escape sequences from `Drop` can corrupt the terminal
/// state just as easily as omitting them.  `Drop` is the safety net for early
/// returns and panic unwinding; callers may still restore eagerly when another
/// UI needs the terminal first.
trait TerminalRestorer<T> {
    fn restore(&mut self, terminal: &mut T) -> Result<()>;
}

impl<T, F> TerminalRestorer<T> for F
where
    F: for<'a> FnMut(&'a mut T) -> Result<()>,
{
    fn restore(&mut self, terminal: &mut T) -> Result<()> {
        self(terminal)
    }
}

struct TerminalOwner<T, R: TerminalRestorer<T>> {
    terminal: T,
    restorer: R,
    restored: bool,
}

impl<T, R: TerminalRestorer<T>> TerminalOwner<T, R> {
    fn new(terminal: T, restorer: R) -> Self {
        Self {
            terminal,
            restorer,
            restored: false,
        }
    }

    fn terminal_mut(&mut self) -> &mut T {
        &mut self.terminal
    }

    /// Restore the terminal once.  Mark it first so a failed best-effort
    /// restoration is never repeated by `Drop`.
    fn restore_once(&mut self) {
        if std::mem::replace(&mut self.restored, true) {
            return;
        }
        if let Err(error) = self.restorer.restore(&mut self.terminal) {
            tracing::warn!("restore terminal failed: {error}");
        }
    }
}

impl<T, R: TerminalRestorer<T>> Drop for TerminalOwner<T, R> {
    fn drop(&mut self) {
        self.restore_once();
    }
}

struct SessionRestore {
    mode: UiMode,
}

impl TerminalRestorer<Terminal> for SessionRestore {
    fn restore(&mut self, terminal: &mut Terminal) -> Result<()> {
        restore_session_terminal(terminal, self.mode)
    }
}

struct FullscreenRestore;

impl TerminalRestorer<Terminal> for FullscreenRestore {
    fn restore(&mut self, terminal: &mut Terminal) -> Result<()> {
        ui::restore_fullscreen_terminal(terminal)
    }
}

/// The session terminal owns its UI mode and therefore its entire restoration
/// context.  This makes restoration an invariant of terminal ownership rather
/// than a responsibility of every `run_session` exit path.
struct SessionTerminal {
    owner: TerminalOwner<Terminal, SessionRestore>,
}

impl SessionTerminal {
    fn fresh(mode: UiMode) -> Result<Self> {
        Ok(Self {
            owner: TerminalOwner::new(setup_session_terminal(mode)?, SessionRestore { mode }),
        })
    }

    fn terminal_mut(&mut self) -> &mut Terminal {
        self.owner.terminal_mut()
    }

    fn restore_once(&mut self) {
        self.owner.restore_once();
    }
}

impl Drop for SessionTerminal {
    fn drop(&mut self) {
        self.restore_once();
    }
}

type FullscreenTerminal = TerminalOwner<Terminal, FullscreenRestore>;

impl TerminalOwner<Terminal, FullscreenRestore> {
    fn fresh() -> Result<Self> {
        Ok(Self::new(
            ui::setup_fullscreen_terminal()?,
            FullscreenRestore,
        ))
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
        .with_writer(SynchronizedFileWriter::new(file))
        .with_env_filter(filter)
        .with_ansi(false)
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .init();
    Ok(())
}

/// A tracing writer that serializes each complete formatted event.
///
/// `tracing_subscriber` may write a single JSON event in multiple calls, so
/// locking individual writes would still allow records from concurrent tasks
/// to interleave.
#[derive(Clone)]
struct SynchronizedFileWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl SynchronizedFileWriter {
    fn new(file: std::fs::File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }
}

struct LockedFileWriter<'a> {
    file: MutexGuard<'a, std::fs::File>,
}

impl Write for LockedFileWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SynchronizedFileWriter {
    type Writer = LockedFileWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LockedFileWriter {
            file: self
                .file
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsageRefreshTrigger {
    Startup,
    CompletedTurn,
    CodexOnly,
}

fn should_refresh_claude_usage(trigger: UsageRefreshTrigger, completed_turns: u64) -> bool {
    matches!(trigger, UsageRefreshTrigger::Startup)
        || (matches!(trigger, UsageRefreshTrigger::CompletedTurn)
            && completed_turns.is_multiple_of(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::{
        collections::HashSet,
        panic::{AssertUnwindSafe, catch_unwind},
        sync::atomic::AtomicUsize,
        sync::{Arc, Barrier},
    };

    struct CountRestore(Arc<AtomicUsize>);

    impl TerminalRestorer<()> for CountRestore {
        fn restore(&mut self, _: &mut ()) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn side_runtime_config_has_no_agent_services_or_persistence() {
        let agent = SelectedAgent {
            source_id: "test-agent".to_string(),
            program: PathBuf::from("agent"),
            args: vec!["acp".to_string()],
            env: std::collections::HashMap::new(),
        };
        let cfg = isolated_side_runtime_config(
            &agent,
            Some("child-session".to_string()),
            PathBuf::from("/workspace"),
            vec![PathBuf::from("/extra")],
            None,
            acp::DEFAULT_FS_TEXT_BYTES,
        );

        assert!(cfg.mcp_servers.is_empty());
        assert!(cfg.subagents.is_none());
        assert!(cfg.role_config.is_none());
        assert!(cfg.agent_source_id.is_none());
        assert!(cfg.config_path.is_none());
        assert!(cfg.saved_session_config.is_empty());
        assert!(cfg.side_prompt_policy);
        assert_eq!(cfg.resume_session.as_deref(), Some("child-session"));
    }

    #[test]
    fn terminal_owner_explicit_restore_then_drop_runs_once() {
        let restores = Arc::new(AtomicUsize::new(0));
        let mut terminal = TerminalOwner::new((), CountRestore(Arc::clone(&restores)));

        terminal.restore_once();
        drop(terminal);

        assert_eq!(restores.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn terminal_owner_restores_during_panic_unwind() {
        let restores = Arc::new(AtomicUsize::new(0));

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _terminal = TerminalOwner::new((), CountRestore(Arc::clone(&restores)));
            panic!("test unwind");
        }));

        assert!(panic.is_err());
        assert_eq!(restores.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replacing_an_eagerly_restored_terminal_keeps_owners_independent() {
        let restores = Arc::new(AtomicUsize::new(0));
        let mut terminal = TerminalOwner::new((), CountRestore(Arc::clone(&restores)));
        terminal.restore_once();

        terminal = TerminalOwner::new((), CountRestore(Arc::clone(&restores)));
        drop(terminal);

        assert_eq!(restores.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn synchronized_file_writer_keeps_concurrent_json_events_intact() {
        const THREADS: usize = 8;
        const EVENTS_PER_THREAD: usize = 40;

        let log = tempfile::NamedTempFile::new().expect("create log");
        let writer = SynchronizedFileWriter::new(log.reopen().expect("open log"));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut handles = Vec::new();

        for thread in 0..THREADS {
            let dispatch = dispatch.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    barrier.wait();
                    for event in 0..EVENTS_PER_THREAD {
                        let marker = format!("event-{thread}-{event}");
                        let payload = marker.repeat(4_096);
                        tracing::info!(marker = %marker, payload = %payload, "concurrent log event");
                    }
                });
            }));
        }

        for handle in handles {
            handle.join().expect("logging thread");
        }
        drop(dispatch);

        let contents = std::fs::read_to_string(log.path()).expect("read log");
        let records: Vec<serde_json::Value> = contents
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("valid JSON log record"))
            .collect();
        assert_eq!(records.len(), THREADS * EVENTS_PER_THREAD);

        let markers: HashSet<_> = records
            .iter()
            .map(|record| {
                let marker = record["marker"].as_str().expect("event marker");
                assert_eq!(
                    record["payload"].as_str(),
                    Some(marker.repeat(4_096).as_str())
                );
                marker.to_owned()
            })
            .collect();
        assert_eq!(markers.len(), THREADS * EVENTS_PER_THREAD);
    }

    #[test]
    fn startup_status_is_visible_without_taking_terminal_control() {
        let mut output = Vec::new();
        write_startup_status(&mut output, 1, Duration::from_secs(12)).expect("status");
        clear_startup_status(&mut output).expect("clear");

        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("/ Discovering models... 12s"));
        assert!(!rendered.contains("\x1b[6n"), "must not issue CPR");
        assert!(
            !rendered.contains("\x1b[?1049h"),
            "must not enter the alternate screen"
        );
        assert!(rendered.ends_with("\r\x1b[2K"));
    }

    fn test_roster_agent(model: &str, agent: &str) -> roster::ResolvedAgent {
        roster::ResolvedAgent {
            model: deepswe::Row {
                model: model.to_string(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: model.to_string(),
            launch: roster::AdapterLaunch {
                kind: roster::AdapterKind::Custom,
                source_id: agent.to_string(),
                command: PathBuf::from(agent),
                args: Vec::new(),
                env: Default::default(),
            },
            ranked: true,
            reasoning_effort: None,
        }
    }

    #[test]
    fn clear_boundary_reports_each_reloaded_seat() {
        let codex = test_roster_agent("gpt-test", "codex-acp");
        let claude = test_roster_agent("claude-test", "claude-acp");
        let roster = roster::Roster {
            primary: codex.clone(),
            review_supervisor: None,
            subagent_default: Some(claude.clone()),
            available: vec![codex, claude],
            choices: Vec::new(),
            warnings: Vec::new(),
            inventory: roster::AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
        };

        assert_eq!(
            models_reload_message(&roster),
            "Models reloaded after /clear: primary gpt-test via codex-acp; subagents claude-test via claude-acp"
        );
    }

    #[test]
    fn onboarding_opens_for_fresh_and_outdated_unpinned_sessions() {
        let agent = SelectedAgent {
            source_id: "roster:test".to_string(),
            program: PathBuf::from("test-acp"),
            args: Vec::new(),
            env: Default::default(),
        };
        let resume = ResumeTarget {
            session_id: "session-1".to_string(),
            title: None,
        };

        assert_eq!(
            onboarding_kind(false, 0, None, None),
            Some(onboarding::Kind::Fresh)
        );
        assert_eq!(
            onboarding_kind(true, 0, None, None),
            Some(onboarding::Kind::Upgrade)
        );
        assert_eq!(
            onboarding_kind(true, config::ONBOARDING_CONTENT_VERSION, None, None),
            None
        );
        assert_eq!(onboarding_kind(false, 0, Some(&resume), None), None);
        assert_eq!(onboarding_kind(false, 0, None, Some(&agent)), None);
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
    fn parse_accepts_headless_role_overrides_and_normalizes_none() {
        let cli = Cli::try_parse_from([
            "mj",
            "--print",
            "hello",
            "--model",
            "gpt-test",
            "--review-model",
            "claude-review+high",
            "--subagent-model",
            "disabled",
        ])
        .expect("parse role overrides");

        assert_eq!(cli.model, Some(("gpt-test".to_string(), None)));
        assert_eq!(
            cli.review_model,
            Some(("claude-review".to_string(), Some("high".to_string())))
        );
        assert_eq!(
            cli.subagent_model,
            Some((config::DISABLED_MODEL.to_string(), None))
        );
    }

    #[test]
    fn parse_accepts_role_overrides_after_stdin_print_sentinel() {
        let cli = Cli::try_parse_from([
            "mj",
            "--print",
            "-",
            "--model",
            "gpt-test",
            "--review-model",
            "claude-review",
            "--subagent-model",
            "disabled",
        ])
        .expect("parse role overrides after stdin sentinel");

        assert_eq!(cli.print.as_deref(), Some("-"));
        assert_eq!(cli.model, Some(("gpt-test".to_string(), None)));
        assert_eq!(cli.review_model, Some(("claude-review".to_string(), None)));
        assert_eq!(
            cli.subagent_model,
            Some((config::DISABLED_MODEL.to_string(), None))
        );
    }

    #[test]
    fn parse_rejects_role_overrides_without_print() {
        let error = Cli::try_parse_from(["mj", "--model", "gpt-test"])
            .expect_err("--model must require --print");
        assert!(error.to_string().contains("--print"), "{error}");
    }

    #[test]
    fn parse_model_override_splits_trailing_effort() {
        assert_eq!(
            parse_model_override("custom/bpr-agent/bedrock::openai.gpt-5.6-sol+high"),
            Ok((
                "custom/bpr-agent/bedrock::openai.gpt-5.6-sol".to_string(),
                Some("high".to_string())
            ))
        );
        assert_eq!(
            parse_model_override("custom/bpr-agent/bedrock::us.anthropic.claude-opus-4-8+high"),
            Ok((
                "custom/bpr-agent/bedrock::us.anthropic.claude-opus-4-8".to_string(),
                Some("high".to_string())
            ))
        );
    }

    #[test]
    fn parse_model_override_leaves_effort_less_selectors_unchanged() {
        assert_eq!(
            parse_model_override("custom/bpr-agent/deepseek::deepseek-v4-pro"),
            Ok((
                "custom/bpr-agent/deepseek::deepseek-v4-pro".to_string(),
                None
            ))
        );
    }

    #[test]
    fn parse_model_override_still_rejects_disabled_and_auto() {
        assert!(parse_model_override("disabled").is_err());
        assert!(parse_model_override("none").is_err());
        assert!(parse_model_override("auto").is_err());
    }

    #[test]
    fn parse_optional_role_override_splits_trailing_effort() {
        assert_eq!(
            parse_optional_role_override("custom/bpr-agent/bedrock::openai.gpt-5.6-terra+medium"),
            Ok((
                "custom/bpr-agent/bedrock::openai.gpt-5.6-terra".to_string(),
                Some("medium".to_string())
            ))
        );
    }

    #[test]
    fn parse_optional_role_override_plus_none_maps_to_off_effort_not_disabled() {
        assert_eq!(
            parse_optional_role_override("custom/bpr-agent/bedrock::model+none"),
            Ok((
                "custom/bpr-agent/bedrock::model".to_string(),
                Some("off".to_string())
            ))
        );
    }

    #[test]
    fn parse_optional_role_override_bare_none_and_disabled_still_disable_the_role() {
        assert_eq!(
            parse_optional_role_override("none"),
            Ok((config::DISABLED_MODEL.to_string(), None))
        );
        assert_eq!(
            parse_optional_role_override("disabled"),
            Ok((config::DISABLED_MODEL.to_string(), None))
        );
        assert_eq!(
            parse_optional_role_override("NONE"),
            Ok((config::DISABLED_MODEL.to_string(), None))
        );
    }

    #[test]
    fn parse_optional_role_override_leaves_effort_less_selectors_unchanged() {
        assert_eq!(
            parse_optional_role_override("custom/bpr-agent/deepseek::deepseek-v4-pro"),
            Ok((
                "custom/bpr-agent/deepseek::deepseek-v4-pro".to_string(),
                None
            ))
        );
    }

    #[test]
    fn parse_role_override_effort_is_case_insensitive_and_off_passes_through() {
        assert_eq!(
            parse_optional_role_override("some-model+OFF"),
            Ok(("some-model".to_string(), Some("off".to_string())))
        );
        assert_eq!(
            parse_optional_role_override("some-model+XHIGH"),
            Ok(("some-model".to_string(), Some("xhigh".to_string())))
        );
        assert_eq!(
            parse_optional_role_override("some-model+MAX"),
            Ok(("some-model".to_string(), Some("max".to_string())))
        );
    }

    #[test]
    fn parse_role_override_ignores_unknown_plus_suffix() {
        // A `+` that isn't a known effort token is left as part of the
        // model selector rather than misparsed as an effort split.
        assert_eq!(
            parse_optional_role_override("some-model+not-an-effort"),
            Ok(("some-model+not-an-effort".to_string(), None))
        );
    }

    #[test]
    fn parse_rejects_auto_and_disabled_primary_overrides() {
        for value in ["auto", "disabled", "none"] {
            assert!(
                Cli::try_parse_from(["mj", "--print", "hello", "--model", value]).is_err(),
                "accepted invalid --model override {value}"
            );
            assert!(
                Cli::try_parse_from(["mj", "--print", "hello", "--review-model", value]).is_err(),
                "accepted invalid --review-model override {value}"
            );
        }
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

        let cli = Cli::try_parse_from(["mj", "models", "refresh"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));
    }

    #[test]
    fn parse_accepts_permission_mode_canonical_and_legacy_values() {
        let canonical =
            Cli::try_parse_from(["mj", "--permission-mode", "auto"]).expect("parse canonical");
        assert!(matches!(
            canonical.permission_mode,
            Some(HeadlessPermissionMode::Auto)
        ));

        let legacy =
            Cli::try_parse_from(["mj", "--permission-mode", "acceptEdits"]).expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            Some(HeadlessPermissionMode::Auto)
        ));

        let canonical =
            Cli::try_parse_from(["mj", "--permission-mode", "yolo"]).expect("parse canonical");
        assert!(matches!(
            canonical.permission_mode,
            Some(HeadlessPermissionMode::Yolo)
        ));

        let legacy = Cli::try_parse_from(["mj", "--permission-mode", "bypassPermissions"])
            .expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            Some(HeadlessPermissionMode::Yolo)
        ));

        let legacy =
            Cli::try_parse_from(["mj", "--permission-mode", "default"]).expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            Some(HeadlessPermissionMode::Manual)
        ));
    }

    #[test]
    fn parse_leaves_permission_mode_unset_when_omitted() {
        let cli = Cli::try_parse_from(["mj"]).expect("parse");
        assert!(cli.permission_mode.is_none());
    }

    #[test]
    fn parse_rejects_unknown_permission_mode_value() {
        let err = Cli::try_parse_from(["mj", "--permission-mode", "unsafe"]).expect_err("reject");
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
        assert!(help.contains("[possible values: manual, auto, yolo]"));
        assert!(!help.contains("acceptEdits"));
        assert!(!help.contains("bypassPermissions"));
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
    fn parse_models_refresh_subcommand() {
        let cli = Cli::try_parse_from(["mj", "models", "refresh"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Models(ModelsArgs {
                command: ModelsCommand::Refresh
            }))
        ));

        let error = Cli::try_parse_from(["mj", "models"]).expect_err("refresh is required");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn parse_agents_install_subcommand() {
        let cli = Cli::try_parse_from(["mj", "agents", "install", "--yes"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Agents(AgentsArgs {
                command: AgentsCommand::Install(AgentsInstallArgs { yes: true })
            }))
        ));

        let error = Cli::try_parse_from(["mj", "agents"]).expect_err("install is required");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
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

    #[test]
    fn claude_usage_refreshes_at_startup_then_every_second_completed_turn() {
        let triggers = [
            (UsageRefreshTrigger::Startup, 0),
            (UsageRefreshTrigger::CodexOnly, 0),
            (UsageRefreshTrigger::CompletedTurn, 1),
            (UsageRefreshTrigger::CodexOnly, 1),
            (UsageRefreshTrigger::CompletedTurn, 2),
            (UsageRefreshTrigger::CompletedTurn, 3),
            (UsageRefreshTrigger::CompletedTurn, 4),
        ];
        let refreshes = triggers
            .into_iter()
            .filter_map(|(trigger, completed_turns)| {
                should_refresh_claude_usage(trigger, completed_turns).then_some(completed_turns)
            })
            .collect::<Vec<_>>();

        assert_eq!(refreshes, vec![0, 2, 4]);
    }
}

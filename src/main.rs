//! mjolnir: an interactive terminal client for any ACP-speaking agent.
//!
//! Resolves a model-first agent roster from DeepSWE and locally
//! launchable ACP adapters, then renders the active foreground ACP session in
//! a ratatui chat UI.

mod acp;
mod agent_instructions;
mod agent_usage;
mod app;
mod claude_token;
mod claude_usage;
mod codex_token;
mod codex_usage;
mod config;
#[cfg(test)]
mod deepswe;
mod discrete_review;
#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
use mj_desktop as desktop;
mod event;
mod headless;
mod keep_awake;
mod labels;
mod memory;
mod menu;
mod onboarding;
mod orchestrator;
mod palette;
mod quota;
mod remote;
mod remote_host;
mod roster;
mod self_update;
mod session;
mod side;
mod spinner;
mod subagent;
mod terminal_palette;
mod termination;
mod theme;
mod ui;
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
use crate::palette::TerminalThemeKindExt;
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
    /// independent of the adapter's own default, e.g.
    /// `gpt-5-6-sol+high`.
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
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Install repository guidance for coding agents.
    Agents(AgentsArgs),
    /// Open the remote viewer in a native desktop window.
    #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
    App(AppArgs),
    /// Pipe stdin/stdout to an in-process MCP tool server of a parent mj
    /// process. Spawned by ACP agents as an advertised stdio MCP server;
    /// not for interactive use.
    #[command(hide = true)]
    McpBridge(McpBridgeArgs),
    /// List and manage persistent cross-session memories.
    Memory(MemoryArgs),
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

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
#[derive(Debug, clap::Args)]
struct AppArgs {
    /// Days of disconnected-session history to keep. Pass 0 to retain it
    /// forever.
    #[arg(long, default_value_t = 30)]
    history_days: u32,
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
struct McpBridgeArgs {
    /// Loopback address of the parent mj process's MCP bridge listener.
    #[arg(long)]
    addr: String,
}

#[derive(Debug, clap::Args)]
struct MemoryArgs {
    #[command(subcommand)]
    command: MemoryCommand,
}

#[derive(Debug, clap::Subcommand)]
enum MemoryCommand {
    /// Print every stored memory, grouped by scope.
    List,
    /// Save one memory, scoped to the current project unless --global.
    Add(MemoryAddArgs),
    /// Delete one memory by id.
    Forget(MemoryForgetArgs),
    /// Delete every stored memory in every project.
    Clear(MemoryClearArgs),
}

#[derive(Debug, clap::Args)]
struct MemoryAddArgs {
    /// One short, self-contained fact to remember across sessions.
    #[arg(required = true)]
    text: Vec<String>,
    /// Save for every project instead of scoping to the current one.
    #[arg(long)]
    global: bool,
}

#[derive(Debug, clap::Args)]
struct MemoryForgetArgs {
    /// Memory id as shown in listings (accepts `7` or `m7`).
    id: String,
}

#[derive(Debug, clap::Args)]
struct MemoryClearArgs {
    /// Delete without a confirmation round trip.
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
    /// Probe enabled ACP adapters and report the available model count.
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
/// role-override model selector, e.g. `gpt-5-6-sol+high`.
/// Case-insensitive; `none` canonicalizes to `off`, which explicitly turns
/// reasoning off rather than leaving the adapter's default effort untouched.
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
    /// Deprecated no-op: tailscale is detected automatically. Accepted so
    /// existing invocations keep working.
    #[arg(long, hide = true)]
    tailscale: bool,
    /// Skip tailscale detection and bind loopback only, as if this machine
    /// were not on a tailnet. Without it, `mj server` serves a trusted
    /// certificate for this machine's ts.net name whenever tailscale is
    /// running with MagicDNS and HTTPS Certificates enabled.
    #[arg(long)]
    no_tailscale_detect: bool,
    /// TCP port to listen on. Local `mj` sessions read the running server's
    /// port, so they follow a non-default choice without extra flags.
    #[arg(long, default_value_t = remote::DEFAULT_REMOTE_CONTROL_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
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

/// The chat surface for this run: the configured interface preference, with
/// the `--fullscreen-tui` flag as a one-run fullscreen override.
fn effective_ui_mode(cli_fullscreen_tui: bool, cfg: &Config) -> UiMode {
    ui_mode(cli_fullscreen_tui || cfg.interface == config::InterfaceMode::Fullscreen)
}

/// Best-effort match for the mode the terminal was left in after a session:
/// sessions adopt a changed interface preference on /new, so the freshest
/// config is the closest stand-in for the session that just ended.
fn post_session_ui_mode(cli_fullscreen_tui: bool) -> UiMode {
    effective_ui_mode(
        cli_fullscreen_tui,
        &Config::load(&config::default_config_path()).unwrap_or_default(),
    )
}

fn should_run_startup_update_check(cli: &Cli) -> bool {
    if cli.no_update_check || cli.print.is_some() {
        return false;
    }
    match &cli.command {
        Some(Commands::Agents(_)) => false,
        #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
        Some(Commands::App(_)) => false,
        Some(Commands::McpBridge(_)) => false,
        Some(Commands::Memory(_)) => false,
        Some(Commands::Models(_)) => false,
        Some(Commands::Resume(args)) => !args.list,
        Some(Commands::Server(_)) => false,
        None => true,
    }
}

fn run_memory_command(command: MemoryCommand, cwd: &Path) -> Result<()> {
    let store = memory::default_path();
    match command {
        MemoryCommand::List => {
            let memory_config = Config::load(&config::default_config_path())
                .map(|config| config.memory)
                .unwrap_or_default();
            println!("{}", memory::render_full_list(&store, &memory_config));
            Ok(())
        }
        MemoryCommand::Add(args) => {
            let text = args.text.join(" ");
            let scope = (!args.global).then(|| memory::project_key(cwd));
            let entry = memory::add(&store, &text, scope.clone())?;
            match scope {
                Some(project) => println!("Saved memory m{} for {}.", entry.id, project.display()),
                None => println!("Saved memory m{} (global).", entry.id),
            }
            Ok(())
        }
        MemoryCommand::Forget(args) => {
            let id = args
                .id
                .strip_prefix('m')
                .unwrap_or(&args.id)
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("invalid memory id: {}", args.id))?;
            match memory::forget(&store, id)? {
                Some(entry) => {
                    println!("Forgot memory m{}: {}", entry.id, entry.text);
                    Ok(())
                }
                None => Err(anyhow::anyhow!("no memory with id m{id}")),
            }
        }
        MemoryCommand::Clear(args) => {
            if !args.yes {
                let count = memory::entries(&store)
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                println!(
                    "This deletes all {} across every project. \
                     Re-run with --yes to proceed.",
                    memory::count_label(count)
                );
                return Ok(());
            }
            let removed = memory::clear(&store)?;
            println!("Cleared {}.", memory::count_label(removed));
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_file.as_deref())?;
    // The bridge child must stay a bare stdio pipe: no signal coordinator,
    // no update check, nothing that could write to stdout.
    if let Some(Commands::McpBridge(args)) = &cli.command {
        return mj_core::mcp_bridge::run_bridge(&args.addr).await;
    }
    // Register the platform adapter before config load or roster resolution.
    #[cfg(target_os = "android")]
    mj_anvil::register();
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
            #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
            Commands::App(args) => {
                run_desktop_app(
                    args,
                    cwd,
                    top_level_additional_directories,
                    snapshot_exclusions,
                    fs_max_text_bytes,
                    termination.token(),
                )
                .await
            }
            // Dispatched before the termination coordinator installs; kept
            // here only for match exhaustiveness.
            Commands::McpBridge(args) => mj_core::mcp_bridge::run_bridge(&args.addr).await,
            Commands::Memory(args) => run_memory_command(args.command, &cwd),
            Commands::Models(args) => match args.command {
                ModelsCommand::Refresh => {
                    let config_path = config::default_config_path();
                    let mut cfg = Config::load(&config_path)?;
                    let (roster, notices) = roster::resolve_recovering(&mut cfg, &cwd).await?;
                    if !notices.is_empty()
                        && let Err(error) = cfg.save(&config_path)
                    {
                        eprintln!("warning: model recovery notices were not persisted: {error:#}");
                    }
                    println!(
                        "Probed enabled ACP adapters; {} models available.",
                        available_model_count(&roster)
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
                    tailscale_detect: !args.no_tailscale_detect,
                    port: args.port,
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

    // Ask the terminal what its own foreground and background are before
    // anything draws. The palette needs the real background to blend diff rows
    // against it; without an answer every fill is dropped in favour of
    // foreground-only styling, so a silent terminal costs appearance, not
    // correctness.
    //
    // Deliberately placed after the subcommand and headless paths have had
    // their chance to return: those never draw a palette, and probing there
    // would spend ~120ms and write escape sequences to a terminal that is
    // about to be handed back to the shell.
    terminal_palette::set_default_colors(terminal_palette::probe_default_colors());

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
        fullscreen_tui,
    )
    .await;

    let mode = post_session_ui_mode(fullscreen_tui);
    let worktree_kept = handle_worktree_after_tui(worktree.as_ref(), Some(mode));

    // Print resume hint so the user can come back to this session.
    match &result {
        Ok(Some(session_id)) => {
            if worktree_kept {
                print_resume_hint(
                    mode,
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

fn available_model_count(roster: &roster::Roster) -> usize {
    roster
        .available
        .iter()
        .map(|role| role.model.model.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
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
                    if let Some(record) =
                        mj_core::session_provenance::find(&entry.session_id, &entry.cwd)
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

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
async fn run_desktop_app(
    args: AppArgs,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    snapshot_exclusions: Vec<PathBuf>,
    fs_max_text_bytes: u64,
    termination: CancellationToken,
) -> Result<()> {
    let workspace_roots = validate_workspace_roots(&cwd, &additional_directories)?;
    let config_path = config::default_config_path();
    let mut cfg =
        Config::load(&config_path).with_context(|| format!("load {}", config_path.display()))?;
    cfg.apply_default_team();
    let resolved = match roster::resolve(&cfg, &cwd).await {
        Ok(roster) => Ok(roster),
        Err(error) => match error.downcast_ref::<mj_core::roster::NothingLaunchable>() {
            Some(nothing) => Err(remote::SetupPending(nothing.message.clone())),
            None => return Err(error),
        },
    };
    let session_manager = desktop_session_manager(
        &resolved,
        remote_host::config_file_hash(&config_path),
        &cwd,
        workspace_roots.additional_directories(),
        &snapshot_exclusions,
        fs_max_text_bytes,
    );

    let server_stop = termination.child_token();
    let (handle, serve) = remote::prepare_desktop_server(remote::DesktopServerOptions {
        config: cfg,
        roster: resolved,
        history_days: args.history_days,
        cwd,
        additional_directories: workspace_roots.additional_directories().to_vec(),
        snapshot_exclusions,
        fs_max_text_bytes,
        session_manager,
        termination: server_stop.clone(),
    })
    .await?;

    let (failure_tx, failure_rx) = tokio::sync::oneshot::channel::<String>();
    let server_task = tokio::spawn({
        let server_stop = server_stop.clone();
        async move {
            let result = serve.await;
            if !server_stop.is_cancelled() {
                let message = match &result {
                    Ok(()) => "desktop server exited unexpectedly".to_string(),
                    Err(error) => format!("desktop server failed: {error:#}"),
                };
                let _ = failure_tx.send(message);
            }
            result
        }
    });
    let (shell_tx, shell_rx) = tokio::sync::oneshot::channel::<desktop::DesktopShellRemote>();
    let watchdog = tokio::spawn({
        let termination = termination.clone();
        async move {
            let failure = tokio::select! {
                _ = termination.cancelled() => None,
                failure = failure_rx => match failure {
                    Ok(message) => Some(message),
                    Err(_) => return,
                },
            };
            let Ok(shell) = shell_rx.await else {
                return;
            };
            match failure {
                Some(message) => shell.fail(message),
                None => shell.close(),
            }
        }
    });

    println!("Opening the Mjolnir desktop viewer at {}", handle.origin);
    let shell_result = desktop::run(
        desktop::DesktopShellOptions {
            origin: handle.origin,
            certificate_der: handle.certificate_der,
            bootstrap_cookie_name: handle.bootstrap_cookie_name,
            bootstrap_cookie_value: handle.bootstrap_cookie_value,
        },
        move |shell| {
            let _ = shell_tx.send(shell);
        },
    );

    server_stop.cancel();
    let serve_result = server_task.await.context("join desktop server")?;
    watchdog.abort();
    match (shell_result, serve_result) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(shell_error), _) => Err(shell_error),
        (Ok(_), Err(serve_error)) => Err(serve_error),
    }
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
fn desktop_session_manager(
    resolved: &std::result::Result<roster::Roster, remote::SetupPending>,
    config_hash: Option<u64>,
    cwd: &Path,
    additional_directories: &[PathBuf],
    snapshot_exclusions: &[PathBuf],
    fs_max_text_bytes: u64,
) -> Arc<remote_host::RootServerSessionManager> {
    Arc::new(match resolved {
        Ok(roster) => remote_host::RootServerSessionManager::new_roster(
            roster.clone(),
            config_hash,
            cwd.to_path_buf(),
            additional_directories.to_vec(),
            snapshot_exclusions.to_vec(),
            fs_max_text_bytes,
        ),
        Err(remote::SetupPending(reason)) => remote_host::RootServerSessionManager::new_unresolved(
            reason.clone(),
            config_hash,
            cwd.to_path_buf(),
            additional_directories.to_vec(),
            snapshot_exclusions.to_vec(),
            fs_max_text_bytes,
        ),
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
    let mode = effective_ui_mode(args.fullscreen_tui, &cfg);
    let mut resume_roster = if args.list {
        roster::resolve(&cfg, &cwd).await?
    } else {
        with_startup_spinner(roster::resolve(&cfg, &cwd)).await?
    };
    let mut agent = selected_agent_for_role(&resume_roster.primary);
    if let Some(session_id) = args.session_id.as_deref()
        && let Some(record) = mj_core::session_provenance::find(session_id, &cwd)
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
        crate::roster::rebind_auto_review_for_primary(&mut resume_roster, &cfg);
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
                mj_core::session_provenance::record(mj_core::session_provenance::Record {
                    session_id: session_id.to_string(),
                    cwd: entry.cwd.clone(),
                    adapter_source_id: role.launch.source_id.clone(),
                    model: role.model.model.clone(),
                    model_value: role.model_value.clone(),
                });
                agent = selected_agent_for_role(&role);
                resume_roster.primary = role;
                crate::roster::rebind_auto_review_for_primary(&mut resume_roster, &cfg);
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
            args.fullscreen_tui,
        )
        .await;
        let mode = post_session_ui_mode(args.fullscreen_tui);
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
                crate::roster::rebind_auto_review_for_primary(&mut resume_roster, &cfg);
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
                    args.fullscreen_tui,
                )
                .await;
                let mode = post_session_ui_mode(args.fullscreen_tui);
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
    let mut prompt = String::new();
    read_headless_prompt_from(&mut std::io::stdin(), &mut prompt)?;
    Ok(prompt)
}

fn read_headless_prompt_from(reader: &mut impl std::io::Read, prompt: &mut String) -> Result<()> {
    reader
        .read_to_string(prompt)
        .context("read prompt from stdin")?;
    Ok(())
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
) -> Result<mj_core::paths::WorkspaceRoots> {
    mj_core::paths::WorkspaceRoots::new(cwd, additional_directories)
}

fn worktree_label(worktree: Option<&CreatedWorktree>) -> Option<String> {
    worktree.map(|w| mj_core::paths::folder_label(&w.worktree_root))
}

fn project_label(cwd: &std::path::Path) -> String {
    mj_core::paths::display_path_with_tilde(cwd)
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

/// Inputs shared by the primary session's long-lived subagent MCP endpoint.
/// The endpoint can replace its launch configuration without replacing the
/// primary ACP session.
#[derive(Clone)]
pub(crate) struct LiveSubagentOptions {
    pub(crate) agent_stderr: Option<PathBuf>,
    pub(crate) snapshot_exclusions: Vec<PathBuf>,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) fs_max_text_bytes: u64,
    pub(crate) session_tag: String,
    pub(crate) handoff_counter: Arc<AtomicUsize>,
    pub(crate) id_allocator: subagent::SubagentIdAllocator,
    pub(crate) active_workers: subagent::ActiveSubagentWorkers,
    pub(crate) reports: subagent::SubagentReportBus,
    pub(crate) runs: subagent::SubagentRegistry,
}

pub(crate) fn configured_subagent_service(
    pool: quota::RolePool,
    options: &LiveSubagentOptions,
    config: &config::SubagentsConfig,
) -> subagent::Config {
    let mut service = subagent::Config::new(pool, options.agent_stderr.clone());
    if let Some(role) = service.role_config.as_mut() {
        role.session_tag = Some(options.session_tag.clone());
    }
    service
        .with_subagent_handoff_counter(options.handoff_counter.clone())
        .with_id_allocator(options.id_allocator.clone())
        .with_active_implementation_workers(options.active_workers.clone())
        .with_max_parallel(config.max_parallel)
        .with_debrief(config.debrief)
        .with_permission_mode(config.permission)
        .with_reports(options.reports.clone())
        .with_run_registry(options.runs.clone())
        .with_prewarm(subagent::RunContext {
            cwd: options.cwd.clone(),
            additional_directories: options.additional_directories.clone(),
            snapshot_exclusions: options.snapshot_exclusions.clone(),
            fs_max_text_bytes: options.fs_max_text_bytes,
            access_mode: acp::RuntimeAccessMode::Full,
        })
}

/// Preserve the resolver's original error when a review cannot construct its
/// specialist fan-out. The orchestrator must never receive a bare `None` and
/// invent an explanation later.
pub(crate) fn review_fanout_error(
    workers_available: bool,
    supervisor_available: bool,
    subagents_model: &str,
    discrete_review: bool,
    roster_warnings: &[String],
) -> String {
    let mut causes = Vec::new();
    if !workers_available {
        if matches!(subagents_model, config::DISABLED_MODEL | "none") {
            causes.push("`subagents.model` is disabled in the active configuration".to_string());
        } else if let Some(warning) = roster_warnings
            .iter()
            .find(|warning| warning.starts_with("subagent delegation is disabled:"))
        {
            causes.push(warning.clone());
        }
    }
    if !supervisor_available {
        if !discrete_review {
            causes.push(
                "`agent.discrete_review` is disabled in the active configuration".to_string(),
            );
        } else if let Some(warning) = roster_warnings
            .iter()
            .find(|warning| warning.starts_with("agentic review supervisor is disabled:"))
        {
            causes.push(warning.clone());
        }
    }
    causes.extend(
        roster_warnings
            .iter()
            .filter(|warning| warning.contains(" unavailable: "))
            .cloned(),
    );
    causes.sort();
    causes.dedup();
    assert!(
        !causes.is_empty(),
        "roster resolution did not record why the review fan-out is unavailable"
    );
    causes.join("\n")
}

pub(crate) fn primary_route_matches(
    active: &roster::ResolvedAgent,
    candidate: &roster::ResolvedAgent,
) -> bool {
    active.launch.source_id == candidate.launch.source_id
        && active.model.model == candidate.model.model
        && active.model_value == candidate.model_value
        && active.reasoning_effort == candidate.reasoning_effort
}

struct RunSessionResult {
    reason: UiExitReason,
    session_id: Option<String>,
    session_title: Option<String>,
    theme_kind: theme::TerminalThemeKind,
    spinner_style: spinner::SpinnerStyle,
    primary_session_handoff: Option<String>,
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

impl From<ui::UiRunResult> for RunSessionResult {
    fn from(result: ui::UiRunResult) -> Self {
        Self {
            reason: result.reason,
            session_id: result.session_id,
            session_title: result.session_title,
            theme_kind: result.theme_kind,
            spinner_style: result.spinner_style,
            primary_session_handoff: result.primary_session_handoff,
        }
    }
}

fn apply_session_result_to_config(cfg: &mut Config, result: &RunSessionResult) {
    cfg.theme = result.theme_kind;
    cfg.spinner = result.spinner_style;
}

async fn resolve_roster_for_tui(
    cfg: &mut Config,
    cwd: &Path,
) -> Result<(roster::Roster, Vec<String>)> {
    with_startup_spinner(roster::resolve_recovering(cfg, cwd)).await
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
    fullscreen_tui: bool,
) -> Result<Option<String>> {
    let termination = runtime_options.termination.clone();
    let config_path = config::default_config_path();
    let config_exists = config::Config::path_has_saved_config(&config_path);
    let mut cfg = Config::load(&config_path)?;
    // A machine signed in to both providers starts on the default team
    // instead of being asked to pick one.
    cfg.apply_default_team();
    let team_selection_required = !config::has_valid_team(&cfg);
    let onboarding_kind = onboarding_kind(
        config_exists,
        cfg.onboarding_version,
        team_selection_required,
        resume_target.as_ref(),
        initial_agent.as_ref(),
    );
    let mut roster = if let Some(kind) = onboarding_kind {
        let initial_resolution = resolve_roster_for_tui(&mut cfg, &cwd)
            .await
            .map(|(roster, _)| roster);
        let Some((accepted_config, accepted_roster)) = run_startup_onboarding(
            kind,
            cfg,
            initial_resolution.ok(),
            &config_path,
            &cwd,
            termination.clone(),
            team_recovery_notice(config_exists, team_selection_required),
        )
        .await?
        else {
            return Ok(None);
        };
        cfg = accepted_config;
        accepted_roster
    } else {
        let (roster, notices) = resolve_roster_for_tui(&mut cfg, &cwd).await?;
        if !notices.is_empty()
            && let Err(error) = cfg.save(&config_path)
        {
            tracing::warn!(%error, "model recovery notices were not persisted");
        }
        roster
    };
    if let Some(agent) = initial_agent.as_ref()
        && let Some(pinned) = roster.available.iter().find(|role| {
            role.launch.command == agent.program
                && role.launch.args == agent.args
                && role.model.model == agent.source_id.trim_start_matches("roster:")
        })
    {
        roster.primary = pinned.clone();
        crate::roster::rebind_auto_review_for_primary(&mut roster, &cfg);
    }
    let mut primary_agent = selected_agent_for_role(&roster.primary);
    // Computed after onboarding so a freshly picked interface preference
    // shapes the very first session.
    let mut mode = effective_ui_mode(fullscreen_tui, &cfg);

    // Consume resume_session and any pinned resume launch on the first
    // iteration only. Fresh sessions always use the resolved primary agent.
    let mut initial_resume = resume_target;
    let mut initial_agent = initial_agent.or_else(|| Some(primary_agent.clone()));
    let mut pending_new_session_boundary = false;
    let mut pending_models_boundary = None;
    let mut pending_primary_session_handoff = None;
    let mut pending_session_title = None;
    loop {
        let resume = initial_resume.take();
        let primary_session_handoff = pending_primary_session_handoff.take();
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
                session_title: resume
                    .as_ref()
                    .and_then(|target| target.title.clone())
                    .or_else(|| pending_session_title.take()),
            },
            resume.as_ref().map(|target| target.session_id.clone()),
            primary_session_handoff,
            mode,
            cfg.theme,
            cfg.spinner,
            session_boundary,
            roster.clone(),
            cfg.agent.clone(),
            cfg.review.clone(),
            cfg.subagents.clone(),
            termination.clone(),
        )
        .await?;
        apply_session_result_to_config(&mut cfg, &session_result);
        match session_result.reason {
            UiExitReason::Quit => return Ok(session_result.session_id),
            UiExitReason::NewSession | UiExitReason::ClearSession => {
                let show_new_session_boundary = session_result.reason == UiExitReason::NewSession;
                cfg = Config::load(&config_path)?;
                let (resolved, notices) = resolve_roster_for_tui(&mut cfg, &cwd).await?;
                if !notices.is_empty()
                    && let Err(error) = cfg.save(&config_path)
                {
                    tracing::warn!(%error, "model recovery notices were not persisted");
                }
                roster = resolved;
                primary_agent = selected_agent_for_role(&roster.primary);
                mode = effective_ui_mode(fullscreen_tui, &cfg);
                initial_agent = Some(primary_agent.clone());
                pending_new_session_boundary = show_new_session_boundary;
                if session_result.reason == UiExitReason::ClearSession {
                    pending_models_boundary = Some(models_reload_message(&roster));
                }
                continue;
            }
            UiExitReason::TransferSession => {
                let previous_primary = roster.primary.clone();
                let handoff_loaded = session_result.primary_session_handoff.is_some();
                cfg = Config::load(&config_path)?;
                let (resolved, notices) = resolve_roster_for_tui(&mut cfg, &cwd).await?;
                if !notices.is_empty()
                    && let Err(error) = cfg.save(&config_path)
                {
                    tracing::warn!(%error, "model recovery notices were not persisted");
                }
                let new_primary = resolved.primary.clone();
                roster = resolved;
                primary_agent = selected_agent_for_role(&roster.primary);
                mode = effective_ui_mode(fullscreen_tui, &cfg);
                initial_agent = Some(primary_agent.clone());
                pending_primary_session_handoff = session_result.primary_session_handoff;
                pending_session_title = session_result.session_title;
                pending_models_boundary = Some(if handoff_loaded {
                    format!(
                        "Primary switched from {} to {}; session transcript is loading into the new session.",
                        previous_primary.launch.kind.display_name(),
                        new_primary.launch.kind.display_name(),
                    )
                } else {
                    format!(
                        "Primary switched from {} to {}.",
                        previous_primary.launch.kind.display_name(),
                        new_primary.launch.kind.display_name(),
                    )
                });
                continue;
            }
            UiExitReason::SwitchSession => {
                if let Some(session_id) = session_result.session_id {
                    let resume_agent = mj_core::session_provenance::find(&session_id, &cwd)
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

/// The notice shown when a *saved* configuration no longer maps to one of the
/// four Teams. A fresh install has no previous configuration: it gets
/// onboarding's own "choose a Team" prompt instead, and must not be told its
/// (nonexistent) configuration failed to map — that also keeps it on the fresh
/// flow rather than the recovery flow the notice selects.
fn team_recovery_notice(config_exists: bool, team_selection_required: bool) -> Option<String> {
    (config_exists && team_selection_required).then(|| {
        "Your previous configuration does not map to a supported Team. Choose one of the four Teams to continue."
            .to_string()
    })
}

fn onboarding_kind(
    config_exists: bool,
    onboarding_version: u32,
    team_selection_required: bool,
    resume_target: Option<&ResumeTarget>,
    initial_agent: Option<&SelectedAgent>,
) -> Option<onboarding::Kind> {
    if resume_target.is_some() || initial_agent.is_some() {
        return None;
    }
    if !config_exists {
        return Some(onboarding::Kind::Fresh);
    }
    (team_selection_required || onboarding_version < config::ONBOARDING_CONTENT_VERSION)
        .then_some(onboarding::Kind::Upgrade)
}

async fn run_startup_onboarding(
    kind: onboarding::Kind,
    candidate: Config,
    preview: Option<roster::Roster>,
    config_path: &Path,
    cwd: &Path,
    termination: CancellationToken,
    notice: Option<String>,
) -> Result<Option<(Config, roster::Roster)>> {
    let outcome = run_onboarding_once(kind, candidate, preview, notice, cwd, termination).await?;
    match outcome {
        onboarding::Outcome::Accept(next, resolved) => {
            let next = *next;
            let mut resolved = *resolved;
            // A failed save must not abort the session the user just
            // configured; the accepted config still drives it in memory.
            if let Err(error) = next.save(config_path) {
                resolved
                    .warnings
                    .push(format!("Setup choices were not saved: {error:#}"));
                resolved.warnings.sort();
            }
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
                mj_core::session_provenance::record(mj_core::session_provenance::Record {
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
            mj_core::session_provenance::remove(&session_id, &cwd, adapter_source_id.as_deref());
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

fn adapter_source_id_for_ui(role: &roster::ResolvedAgent) -> String {
    role.launch.source_id.clone()
}

/// Adapter kind of the agent this session actually launches. Resume and
/// session-switch flows can route a session to a different adapter than
/// `roster.primary` without re-resolving the roster, so memory gating must
/// follow the launch itself; a launch not found in the roster yields `None`.
///
/// Launched agents encode their roster model as `roster:<model>`
/// (`selected_agent_for_role`), and the match requires model, command, args,
/// and env: distinct routes that merely share a launch command — a custom
/// server wrapping the codex binary, say — must never be conflated.
fn launched_adapter_kind(
    roster: &roster::Roster,
    agent: &SelectedAgent,
) -> Option<roster::AdapterKind> {
    let model = agent.source_id.strip_prefix("roster:")?;
    let matches = |role: &roster::ResolvedAgent| {
        role.model.model == model
            && role.launch.command == agent.program
            && role.launch.args == agent.args
            && role.launch.env == agent.env
    };
    if matches(&roster.primary) {
        return Some(roster.primary.launch.kind);
    }
    roster
        .available
        .iter()
        .find(|role| matches(role))
        .map(|role| role.launch.kind)
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    agent: &SelectedAgent,
    cwd: PathBuf,
    runtime_options: RuntimeOptions,
    header_labels: HeaderLabels,
    resume_session: Option<String>,
    mut primary_session_handoff: Option<String>,
    mode: UiMode,
    mut theme_kind: theme::TerminalThemeKind,
    mut spinner_style: spinner::SpinnerStyle,
    mut session_boundary: Option<String>,
    roster: roster::Roster,
    agent_config: config::AgentConfig,
    review_config: config::ReviewConfig,
    subagents_config: config::SubagentsConfig,
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
    let (subagent_roles, subagent_codex_home) =
        isolated_subagent_roles(crate::roster::subagent_failover_roles(&roster), "subagent")?;

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
    let live_subagent_options = LiveSubagentOptions {
        agent_stderr: runtime_options.agent_stderr.clone(),
        snapshot_exclusions: runtime_options.snapshot_exclusions.clone(),
        cwd: cwd.clone(),
        additional_directories: runtime_options.additional_directories.clone(),
        fs_max_text_bytes: runtime_options.fs_max_text_bytes,
        session_tag: session_tag.clone(),
        handoff_counter: subagent_handoffs_this_turn.clone(),
        id_allocator: subagent_ids.clone(),
        active_workers: active_implementation_workers.clone(),
        reports: subagent_reports.clone(),
        runs: subagent_runs.clone(),
    };
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
            // Pushes the steward's next attempt out when a refresh does
            // not take (e.g. signed out) so the timer cannot spin while
            // the token sits inside the refresh window.
            let mut steward_not_before = tokio::time::Instant::now();
            loop {
                let claude_steward_at = claude_usage_env
                    .as_ref()
                    .map(|env| tokio::time::Instant::now() + claude_token::steward_delay(env));
                let codex_steward_at = codex_usage_env
                    .as_ref()
                    .map(|env| tokio::time::Instant::now() + codex_token::steward_delay(env));
                let steward_at = match (claude_steward_at, codex_steward_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) | (None, Some(a)) => Some(a),
                    (None, None) => None,
                }
                .map(|at| at.max(steward_not_before));
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
                    // Proactive token steward: rotate the Claude and codex
                    // OAuth tokens as they enter their refresh windows so
                    // running seats and every other process on this machine
                    // never meet an expired credential.
                    _ = tokio::time::sleep_until(steward_at.unwrap_or_else(tokio::time::Instant::now)),
                        if steward_at.is_some() =>
                    {
                        if let Some(env) = claude_usage_env.as_ref() {
                            claude_token::ensure_fresh_before_spawn(usage_cwd.clone(), env).await;
                        }
                        if let Some(env) = codex_usage_env.as_ref() {
                            codex_token::ensure_fresh_before_spawn(usage_cwd.clone(), env).await;
                        }
                        steward_not_before = tokio::time::Instant::now()
                            + std::time::Duration::from_secs(10 * 60);
                        continue;
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
    // Always advertise the auxiliary MCP endpoint. A same-primary team change
    // can then add reviewers or subagents to a session that originally had
    // neither, without replacing the primary ACP process.
    let live_subagent_service = match subagent_pool.clone() {
        Some(pool) => subagent::LiveRuntimeService::new(configured_subagent_service(
            pool,
            &live_subagent_options,
            &subagents_config,
        )),
        None => subagent::LiveRuntimeService::unconfigured(),
    };
    let runtime_subagents =
        Some(Arc::new(live_subagent_service.clone()) as Arc<dyn acp::RuntimeService>);

    let mut primary_env = agent.env.clone();
    let primary_permission = runtime_options.permission_mode.and_then(|mode| {
        roster::configure_permissions(roster.primary.launch.kind, mode, &mut primary_env)
    });
    let memory_config = Config::load(&config::default_config_path())
        .map(|config| config.memory)
        .unwrap_or_default();
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
            permission: primary_permission,
            session_tag: Some(session_tag.clone()),
            reasoning_effort: roster.primary.reasoning_effort.clone(),
        }),
        subagents: runtime_subagents,
        memory: memory::SessionMemory::from_config(
            &memory_config,
            &cwd,
            launched_adapter_kind(&roster, agent),
        ),
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
    // Registry source id for the active adapter. `agent.source_id` identifies
    // the selected roster model instead and cannot address ACP inventory.
    let agent_source_id = Some(adapter_source_id_for_ui(&roster.primary));
    let tracker_project_label = header_labels.project.clone();
    // `-w` sessions carry the worktree name in the header; sessions launched
    // directly inside a worktree derive it from cwd so remote viewers badge
    // both the same way.
    let tracker_worktree_label = header_labels
        .worktree
        .clone()
        .or_else(|| mj_core::paths::worktree_name_from_cwd(&cwd));
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
        true,
    );
    let orchestrated = orchestrator::spawn(
        runtime_event_rx,
        orchestrator::Config {
            runtime_commands: runtime_cmd_tx.clone(),
            active_subagent_workers: active_implementation_workers.clone(),
            subagent_reports: subagent_report_rx,
            subagent_report_bus: subagent_reports.clone(),
            subagent_runs: mj_core::orchestrator::SubagentProgressService::new(subagent_runs),
            progress_wake: orchestrator::progress_wake_interval(
                subagents_config.progress_wake_minutes,
            ),
            discrete_review: agent_config.discrete_review,
            review_tier: agent_config.review_tier,
            correction_threshold: agent_config.correction_threshold,
            max_correction_rounds: agent_config.max_correction_rounds,
            primary_model: Some(roster.primary.model.model.clone()),
            review_root: cwd.clone(),
            review_fanout: match (review_workers, roster.review_supervisor.clone()) {
                (Some(workers), Some(supervisor)) => {
                    mj_core::orchestrator::ReviewFanout::available(discrete_review::live_spawner(
                        discrete_review::FanoutConfig {
                            workers,
                            supervisor,
                            cwd: cwd.clone(),
                            additional_directories: runtime_options.additional_directories.clone(),
                            session_tag: Some(session_tag.clone()),
                            agent_stderr: runtime_options.agent_stderr.clone(),
                            snapshot_exclusions: runtime_options.snapshot_exclusions.clone(),
                            fs_max_text_bytes: runtime_options.fs_max_text_bytes,
                            permission: review_config.permission,
                            id_allocator: subagent_ids.clone(),
                        },
                    ))
                }
                (workers, supervisor) => {
                    mj_core::orchestrator::ReviewFanout::unavailable(review_fanout_error(
                        workers.is_some(),
                        supervisor.is_some(),
                        &subagents_config.model,
                        agent_config.discrete_review,
                        &roster.warnings,
                    ))
                }
            },
        },
    );
    let primary_orchestrator = orchestrated.handle.clone();
    let refresh_usage_on_failure = roster.primary.launch.source_id == "codex-acp";
    let event_usage_turn_tx = usage_turn_tx.clone();
    let event_tracker = remote_tracker.clone();
    let event_primary = roster.primary.clone();
    let event_cwd = cwd.clone();
    let (side_ui_event_tx, mut side_ui_event_rx) = mpsc::unbounded_channel();
    let side_event_tracker = remote_tracker.clone();
    let side_events_for_ui = ui_event_tx.clone();
    let side_event_proxy = tokio::spawn(async move {
        while let Some(event) = side_ui_event_rx.recv().await {
            let event = side_event_tracker.intercept_event(event);
            side_event_tracker.observe_event(&event);
            if side_events_for_ui.send(event).is_err() {
                break;
            }
        }
    });
    let event_proxy = tokio::spawn(async move {
        let mut events = orchestrated.events;
        while let Some(event) = events.recv().await {
            if let UiEvent::SessionStarted { session_id, .. } = &event {
                mj_core::session_provenance::record(mj_core::session_provenance::Record {
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
    // Two reads can overlap and finish out of order; the refresher publishes
    // only the newest, because an older worktree state landing over a newer
    // one is exactly the staleness this reader exists to avoid.
    let workspace_diff_refresher = acp::WorkspaceHeadDiffRefresher::new(
        cmd_workspace_roots.clone(),
        cmd_snapshot_exclusions.clone(),
        runtime_options.fs_max_text_bytes,
    );
    let side_agent = agent.clone();
    let side_cwd = cwd.clone();
    let side_additional_directories = runtime_options.additional_directories.clone();
    let side_agent_stderr = runtime_options.agent_stderr.clone();
    let side_fs_max_text_bytes = runtime_options.fs_max_text_bytes;
    let command_primary = roster.primary.clone();
    let command_config_path = config_path.clone();
    let command_quota_gate = quota_gate.clone();
    let command_live_subagent_service = live_subagent_service.clone();
    let command_live_subagent_options = live_subagent_options.clone();
    let mut command_subagent_codex_homes = subagent_codex_home.into_iter().collect::<Vec<_>>();
    let cmd_proxy = tokio::spawn(async move {
        let mut side_runtime: Option<side::Runtime> = None;
        let mut local_epoch = 0_u64;
        while let Some(command) = ui_cmd_rx.recv().await {
            if let UiCommand::StartSide { initial_prompt } = command {
                if side_runtime.is_some() {
                    let _ = side_ui_event_tx.send(UiEvent::Warning(
                        "a side conversation is already active".to_string(),
                    ));
                    continue;
                }
                cmd_tracker.begin_side_start(initial_prompt.is_some());
                let launch = side::Launch {
                    agent: &side_agent,
                    cwd: side_cwd.clone(),
                    additional_directories: side_additional_directories.clone(),
                    agent_stderr: side_agent_stderr.clone(),
                    fs_max_text_bytes: side_fs_max_text_bytes,
                };
                let side =
                    match side::start(launch, &runtime_cmd_tx, side_ui_event_tx.clone()).await {
                        Ok(side) => side,
                        Err(message) => {
                            let event = UiEvent::SideStartFailed { message };
                            let _ = side_ui_event_tx.send(event);
                            continue;
                        }
                    };
                if let Some(text) = initial_prompt {
                    let prompt = UiCommand::SendPrompt {
                        text,
                        images: Vec::new(),
                        resources: Vec::new(),
                    };
                    cmd_tracker.observe_side_command(&prompt);
                    let _ = side.send(prompt);
                }
                side_runtime = Some(side);
                continue;
            }
            if matches!(command, UiCommand::ExitSide) {
                cmd_tracker.finish_side_exit();
                if let Some(side) = side_runtime.take()
                    && let Some(message) =
                        side::discard(side, &side_agent, side_agent_stderr.as_deref()).await
                {
                    let _ = side_ui_event_tx.send(UiEvent::Warning(message));
                }
                continue;
            }
            // Handled before side forwarding: the diff is a property of the
            // workspace on disk, which a side conversation shares, so routing
            // it into a side runtime would only lose it.
            if matches!(command, UiCommand::RefreshWorkspaceDiff) {
                workspace_diff_refresher.spawn(side_ui_event_tx.clone());
                continue;
            }
            let (command, force_main) = match command {
                UiCommand::Main(command) => (*command, true),
                command @ UiCommand::ReloadAuxiliaryAgents => (command, true),
                command => (command, false),
            };
            if !force_main && side_runtime.is_some() {
                if matches!(command, UiCommand::Shutdown) {
                    if let Some(side) = side_runtime.take() {
                        cmd_tracker.finish_side_exit();
                        let _ =
                            side::discard(side, &side_agent, side_agent_stderr.as_deref()).await;
                    }
                } else {
                    let side = side_runtime.as_ref().expect("checked side runtime");
                    cmd_tracker.observe_side_command(&command);
                    let _ = side.send(command);
                    continue;
                }
            }
            cmd_tracker.observe_command(&command);
            if matches!(command, UiCommand::ReloadAuxiliaryAgents) {
                let updated_config = match Config::load(&command_config_path) {
                    Ok(config) => config,
                    Err(error) => {
                        let _ = side_ui_event_tx.send(UiEvent::Warning(format!(
                            "could not apply the saved reviewer configuration: {error:#}"
                        )));
                        continue;
                    }
                };
                let updated_roster = match roster::resolve(&updated_config, &side_cwd).await {
                    Ok(roster) => roster,
                    Err(error) => {
                        let _ = side_ui_event_tx.send(UiEvent::Warning(format!(
                            "the primary session kept its current reviewer configuration because the saved configuration could not be resolved: {error:#}"
                        )));
                        continue;
                    }
                };
                if !primary_route_matches(&command_primary, &updated_roster.primary) {
                    let _ = side_ui_event_tx.send(UiEvent::Info(
                        "primary agent changed; start /new or /clear to apply that route"
                            .to_string(),
                    ));
                    continue;
                }
                let (roles, codex_home) = match isolated_subagent_roles(
                    roster::subagent_failover_roles(&updated_roster),
                    "subagent",
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        let _ = side_ui_event_tx.send(UiEvent::Warning(format!(
                            "could not prepare the saved subagent configuration: {error:#}"
                        )));
                        continue;
                    }
                };
                let pool = (!roles.is_empty()).then(|| {
                    quota::RolePool::new(
                        roles,
                        command_quota_gate.clone(),
                        updated_config.subagents.auto_failover,
                        "subagents",
                        side_ui_event_tx.clone(),
                    )
                });
                let initial_role = pool.as_ref().map(quota::RolePool::current);
                tracing::info!(
                    event = "auxiliary_agents_reconfigured",
                    session_tag = %command_live_subagent_options.session_tag,
                    review_adapter = updated_roster
                        .review_supervisor
                        .as_ref()
                        .map(|role| role.launch.source_id.as_str())
                        .unwrap_or("off"),
                    review_model = updated_roster
                        .review_supervisor
                        .as_ref()
                        .map(|role| role.model.model.as_str())
                        .unwrap_or("off"),
                    subagent_adapter = initial_role
                        .as_ref()
                        .map(|role| role.launch.source_id.as_str())
                        .unwrap_or("off"),
                    subagent_model = initial_role
                        .as_ref()
                        .map(|role| role.model.model.as_str())
                        .unwrap_or("off"),
                    "applied reviewer and subagent configuration without replacing primary"
                );
                if let Some(pool) = pool.as_ref() {
                    command_live_subagent_service
                        .replace(configured_subagent_service(
                            pool.clone(),
                            &command_live_subagent_options,
                            &updated_config.subagents,
                        ))
                        .await;
                } else {
                    command_live_subagent_service.clear();
                }
                if let Some(home) = codex_home {
                    command_subagent_codex_homes.push(home);
                }
                let review_fanout = match (pool, updated_roster.review_supervisor.clone()) {
                    (Some(workers), Some(supervisor)) => {
                        mj_core::orchestrator::ReviewFanout::available(
                            discrete_review::live_spawner(discrete_review::FanoutConfig {
                                workers,
                                supervisor,
                                cwd: side_cwd.clone(),
                                additional_directories: side_additional_directories.clone(),
                                session_tag: Some(
                                    command_live_subagent_options.session_tag.clone(),
                                ),
                                agent_stderr: side_agent_stderr.clone(),
                                snapshot_exclusions: command_live_subagent_options
                                    .snapshot_exclusions
                                    .clone(),
                                fs_max_text_bytes: side_fs_max_text_bytes,
                                permission: updated_config.review.permission,
                                id_allocator: command_live_subagent_options.id_allocator.clone(),
                            }),
                        )
                    }
                    (workers, supervisor) => {
                        mj_core::orchestrator::ReviewFanout::unavailable(review_fanout_error(
                            workers.is_some(),
                            supervisor.is_some(),
                            &updated_config.subagents.model,
                            updated_config.agent.discrete_review,
                            &updated_roster.warnings,
                        ))
                    }
                };
                cmd_orchestrator.set_review_fanout(review_fanout);
                cmd_orchestrator.set_review_enabled(updated_config.agent.discrete_review);
                cmd_orchestrator.set_review_tier(updated_config.agent.review_tier);
                cmd_orchestrator
                    .set_correction_threshold(updated_config.agent.correction_threshold);
                let _ = side_ui_event_tx.send(UiEvent::Info(
                    "reviewer and subagent configuration is active for the current primary session"
                        .to_string(),
                ));
                continue;
            }
            if let UiCommand::SetReviewPolicy {
                enabled,
                tier,
                correction_threshold,
            } = &command
            {
                cmd_orchestrator.set_review_enabled(*enabled);
                cmd_orchestrator.set_review_tier(*tier);
                cmd_orchestrator.set_correction_threshold(*correction_threshold);
                continue;
            }
            if let UiCommand::RunReview { request } = command {
                cmd_orchestrator.request_review(request);
                continue;
            }
            if matches!(command, UiCommand::CompactPrimary) {
                cmd_orchestrator.compact_manual().await;
                continue;
            }
            if let UiCommand::SendPrompt { text, images, .. } = &command {
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
        if let Some(side) = side_runtime.take() {
            cmd_tracker.finish_side_exit();
            if let Some(message) =
                side::discard(side, &side_agent, side_agent_stderr.as_deref()).await
            {
                let _ = side_ui_event_tx.send(UiEvent::Warning(message));
            }
        }
    });

    let mut header_labels = header_labels;
    let ui_result = loop {
        // Reloaded each session so `/settings` edits apply on /new; the
        // default config has every switch below in its enabled state.
        let ui_config = config::Config::load(&config_path).unwrap_or_default();
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
                thought_output: ui_config.thought_output,
                voice_auto_send: ui_config.voice_auto_send,
                feature_hints_enabled: ui_config.feature_hints,
                keep_awake_enabled: ui_config.keep_awake,
                session_boundary: session_boundary.take(),
                primary_session_handoff: primary_session_handoff.take(),
                session_cwd: cwd.clone(),
                additional_workspace_roots: runtime_options.additional_directories.clone(),
                model_choices: roster.choices.clone(),
                acp_inventory: roster.inventory.clone(),
                configured_models: ui_config.model_names(),
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
                review_tier: agent_config.review_tier,
                correction_threshold: agent_config.correction_threshold,
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
                primary_session_handoff: None,
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
                primary_session_handoff: None,
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
                    primary_session_handoff: None,
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
    let side_event_proxy_wait = wait_for_task("side event proxy", side_event_proxy);
    let cmd_proxy_wait = wait_for_task("remote-control command proxy", cmd_proxy);
    if let Some(task) = usage_task {
        tokio::join!(
            event_proxy_wait,
            side_event_proxy_wait,
            cmd_proxy_wait,
            wait_for_task("subscription usage poller", task),
        );
    } else {
        tokio::join!(event_proxy_wait, side_event_proxy_wait, cmd_proxy_wait);
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
    role: roster::ResolvedAgent,
    label: &str,
) -> Result<(roster::ResolvedAgent, Option<tempfile::TempDir>)> {
    if role.launch.kind != roster::AdapterKind::Codex {
        return Ok((role, None));
    }
    let source = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| anyhow::anyhow!("could not locate CODEX_HOME for {label}"))?;
    isolated_subagent_role_from_home(role, label, &source)
}

fn isolated_subagent_role_from_home(
    mut role: roster::ResolvedAgent,
    label: &str,
    source: &Path,
) -> Result<(roster::ResolvedAgent, Option<tempfile::TempDir>)> {
    let isolated = tempfile::Builder::new()
        .prefix(&format!("mj-{label}-codex-"))
        .tempdir()
        .with_context(|| format!("create isolated Codex home for {label}"))?;
    for name in ["config.toml", "models_cache.json", "version.json"] {
        let from = source.join(name);
        if from.is_file() {
            std::fs::copy(&from, isolated.path().join(name)).with_context(|| {
                format!("copy {} into isolated {label} Codex home", from.display())
            })?;
        }
    }
    let source_auth = source.join("auth.json");
    if !source_auth.is_file() {
        anyhow::bail!(
            "Codex is available but {} has no auth.json; sign in from /mjconfig",
            source.display()
        );
    }
    // Credentials must stay shared, never snapshotted: OpenAI rotates refresh
    // tokens, so a private copy goes stale as soon as any other process
    // refreshes or the user signs in again, and the seat then fails every
    // request with "refresh token was revoked" until the session restarts.
    // Codex rewrites auth.json in place, so a symlink keeps the seat on the
    // live grant in both directions.
    share_auth_json(&source_auth, &isolated.path().join("auth.json"), label)?;
    role.launch.env.insert(
        "CODEX_HOME".to_string(),
        isolated.path().display().to_string(),
    );
    Ok((role, Some(isolated)))
}

/// Codex rewrites auth.json in place, so a symlink — or a same-volume hard
/// link on Windows, where symlinks need developer mode or elevation — behaves
/// exactly like the real file. The plain copy is a last resort that reopens
/// the stale-credential window.
fn share_auth_json(source: &Path, target: &Path, label: &str) -> Result<()> {
    let source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&source, target);
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&source, target)
        .or_else(|_| std::fs::hard_link(&source, target))
        .or_else(|_| std::fs::copy(&source, target).map(|_| ()));
    #[cfg(not(any(unix, windows)))]
    let linked = std::fs::copy(&source, target).map(|_| ());
    linked.with_context(|| {
        format!(
            "share {} with the isolated {label} Codex home",
            source.display()
        )
    })
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

fn setup_session_terminal(mode: UiMode) -> Result<mj_tui::Terminal> {
    match mode {
        UiMode::InlineChat => {
            ui::setup_inline_chat_terminal(ui::INLINE_CHAT_HEIGHT).context("setup terminal")
        }
        UiMode::FullscreenTui => ui::setup_fullscreen_terminal().context("setup terminal"),
    }
}

fn restore_session_terminal(terminal: &mut mj_tui::Terminal, mode: UiMode) -> Result<()> {
    match mode {
        UiMode::InlineChat => ui::restore_inline_chat_terminal(terminal),
        UiMode::FullscreenTui => ui::restore_fullscreen_terminal(terminal),
    }
}

type Terminal = mj_tui::Terminal;

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
    request_inline_session_load_with_timeout(
        cmd_tx,
        session_id,
        cwd,
        title,
        Duration::from_secs(15),
    )
    .await
}

async fn request_inline_session_load_with_timeout(
    cmd_tx: &mpsc::UnboundedSender<UiCommand>,
    session_id: String,
    cwd: PathBuf,
    title: Option<String>,
    timeout: Duration,
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
    match tokio::time::timeout(timeout, response).await {
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
    wait_for_task_with_timeout(label, handle, Duration::from_secs(2)).await;
}

async fn wait_for_task_with_timeout(
    label: &str,
    handle: tokio::task::JoinHandle<()>,
    timeout: Duration,
) {
    let abort_handle = handle.abort_handle();
    match tokio::time::timeout(timeout, handle).await {
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
    // Keep the remaining orchestration boundaries explicit: `main`, `run_resume`,
    // `run_app`, and `run_session` own real process-wide configuration, agent
    // subprocesses, and terminal state. Their deterministic decisions are tested
    // here, ACP protocol behavior is tested with mock transports in `acp`, and
    // real terminal restoration is covered by `tests/termination_pty.rs`.
    use super::*;
    use clap::CommandFactory;
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
                kind: roster::AdapterKind::from_source_id(agent)
                    .unwrap_or(roster::AdapterKind::Claude),
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
    fn primary_route_match_requires_the_same_resolved_model() {
        let active = test_roster_agent("gpt-5-6-terra", "codex-acp");
        let same_source_new_model = test_roster_agent("gpt-5-6-sol", "codex-acp");
        let mut same_model_new_effort = active.clone();
        same_model_new_effort.reasoning_effort = Some("high".to_string());

        assert!(!primary_route_matches(&active, &same_source_new_model));
        assert!(!primary_route_matches(&active, &same_model_new_effort));
        assert!(primary_route_matches(&active, &active));
    }

    fn test_roster(
        primary: roster::ResolvedAgent,
        available: Vec<roster::ResolvedAgent>,
    ) -> roster::Roster {
        roster::Roster {
            primary,
            review_supervisor: None,
            subagent_default: None,
            available,
            choices: Vec::new(),
            warnings: Vec::new(),
            inventory: roster::AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
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
    fn clear_boundary_reports_disabled_subagents() {
        let primary = test_roster_agent("gpt-test", "codex-acp");
        let roster = test_roster(primary.clone(), vec![primary]);

        assert_eq!(
            models_reload_message(&roster),
            "Models reloaded after /clear: primary gpt-test via codex-acp; subagents off"
        );
    }

    #[test]
    fn primary_session_routes_are_ranked_unique_and_primary_first() {
        let primary = test_roster_agent("primary", "codex-acp");
        let duplicate = test_roster_agent("duplicate", "codex-acp");
        let alternate = test_roster_agent("alternate", "claude-acp");
        let mut unranked = test_roster_agent("unranked", "opencode");
        unranked.ranked = false;
        let roster = test_roster(
            primary.clone(),
            vec![duplicate, unranked, alternate.clone()],
        );

        let routes = primary_session_routes(&roster);
        let route_models = routes
            .iter()
            .map(|role| role.model.model.as_str())
            .collect::<Vec<_>>();
        assert_eq!(route_models, vec!["primary", "alternate"]);
        assert_eq!(routes[1].launch.source_id, alternate.launch.source_id);
    }

    #[test]
    fn available_model_count_deduplicates_adapter_routes() {
        let primary = test_roster_agent("primary", "codex-acp");
        let duplicate_model = test_roster_agent("primary", "claude-acp");
        let alternate = test_roster_agent("alternate", "claude-acp");
        let roster = test_roster(primary.clone(), vec![primary, duplicate_model, alternate]);

        assert_eq!(available_model_count(&roster), 2);
    }

    #[test]
    fn session_entry_route_prefers_exact_model_then_ranked_adapter_fallback() {
        let primary = test_roster_agent("primary", "codex-acp");
        let exact = test_roster_agent("exact", "claude-acp");
        let fallback = test_roster_agent("fallback", "claude-acp");
        let roster = test_roster(
            primary.clone(),
            vec![primary, fallback.clone(), exact.clone()],
        );
        let entry = |adapter: Option<&str>, model: Option<&str>| session::SessionEntry {
            session_id: "session".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            title: None,
            updated_at: None,
            adapter_source_id: adapter.map(str::to_string),
            model: model.map(str::to_string),
            delete_supported: false,
        };

        assert_eq!(
            role_for_session_entry(&roster, &entry(Some("claude-acp"), Some("exact")))
                .map(|role| role.model.model.as_str()),
            Some("exact")
        );
        assert_eq!(
            role_for_session_entry(&roster, &entry(Some("claude-acp"), Some("missing")))
                .map(|role| role.model.model.as_str()),
            Some("fallback")
        );
        assert!(role_for_session_entry(&roster, &entry(None, Some("exact"))).is_none());
        assert!(role_for_session_entry(&roster, &entry(Some("missing"), None)).is_none());
    }

    #[test]
    fn selected_agent_preserves_resolved_launch() {
        let mut role = test_roster_agent("model", "custom-source");
        role.launch.command = PathBuf::from("/opt/custom agent");
        role.launch.args = vec!["--flag".to_string()];
        role.launch
            .env
            .insert("TOKEN".to_string(), "secret".to_string());

        let selected = selected_agent_for_role(&role);
        assert_eq!(selected.source_id, "roster:model");
        assert_eq!(adapter_source_id_for_ui(&role), "custom-source");
        assert_eq!(selected.program, PathBuf::from("/opt/custom agent"));
        assert_eq!(selected.args, vec!["--flag"]);
        assert_eq!(
            selected.env.get("TOKEN").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn fresh_install_is_not_told_its_previous_configuration_failed_to_map() {
        // No saved config: onboarding's own "choose a Team" prompt applies, and
        // the recovery notice (which also selects the recovery flow) stays off.
        assert_eq!(team_recovery_notice(false, true), None);
        assert_eq!(team_recovery_notice(false, false), None);
        // A saved config that maps to a Team needs no notice either.
        assert_eq!(team_recovery_notice(true, false), None);
        // Only a saved config that no longer maps gets the recovery wording.
        let notice = team_recovery_notice(true, true).expect("recovery notice");
        assert!(notice.starts_with("Your previous configuration does not map"));
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
            onboarding_kind(false, 0, false, None, None),
            Some(onboarding::Kind::Fresh)
        );
        assert_eq!(
            onboarding_kind(true, 0, false, None, None),
            Some(onboarding::Kind::Upgrade)
        );
        assert_eq!(
            onboarding_kind(true, config::ONBOARDING_CONTENT_VERSION, false, None, None),
            None
        );
        assert_eq!(
            onboarding_kind(true, config::ONBOARDING_CONTENT_VERSION, true, None, None),
            Some(onboarding::Kind::Upgrade)
        );
        assert_eq!(onboarding_kind(false, 0, true, Some(&resume), None), None);
        assert_eq!(onboarding_kind(false, 0, true, None, Some(&agent)), None);
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
            mj_core::paths::display_path_with_tilde(&worktree.session_cwd)
        );
    }

    #[test]
    fn project_label_uses_full_directory_path_inside_mjolnir_worktree() {
        let cwd =
            std::path::Path::new("/Users/ryan/code/mjolnir/.mjolnir/worktrees/bold-willow/src");
        assert_eq!(
            project_label(cwd),
            mj_core::paths::display_path_with_tilde(cwd)
        );
    }

    #[test]
    fn project_label_uses_full_directory_path_without_worktree() {
        let cwd = std::path::Path::new("/Users/ryan/code/mjolnir/src");
        assert_eq!(
            project_label(cwd),
            mj_core::paths::display_path_with_tilde(cwd)
        );
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
            theme_kind: theme::TerminalThemeKind::Ansi,
            spinner_style: spinner::SpinnerStyle::Bars,
            primary_session_handoff: None,
        };

        apply_session_result_to_config(&mut cfg, &result);

        assert_eq!(cfg.theme, theme::TerminalThemeKind::Ansi);
        assert_eq!(cfg.spinner, spinner::SpinnerStyle::Bars);
    }

    #[test]
    fn ui_result_conversion_preserves_session_outcome() {
        let result = RunSessionResult::from(ui::UiRunResult {
            reason: UiExitReason::SwitchSession,
            session_id: Some("session-2".to_string()),
            session_title: Some("Selected".to_string()),
            theme_kind: theme::TerminalThemeKind::Adaptive,
            spinner_style: spinner::SpinnerStyle::Globe,
            primary_session_handoff: None,
        });

        assert_eq!(result.reason, UiExitReason::SwitchSession);
        assert_eq!(result.session_id.as_deref(), Some("session-2"));
        assert_eq!(result.session_title.as_deref(), Some("Selected"));
        assert_eq!(result.theme_kind, theme::TerminalThemeKind::Adaptive);
        assert_eq!(result.spinner_style, spinner::SpinnerStyle::Globe);
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
        let cli = try_parse_hermetic(&["mj", "--debug-file", "debug.log"]).expect("parse");
        assert_eq!(cli.log_file, Some(PathBuf::from("debug.log")));

        let cli = try_parse_hermetic(&["mj", "--log-file", "legacy.log"]).expect("parse");
        assert_eq!(cli.log_file, Some(PathBuf::from("legacy.log")));
    }

    #[test]
    fn parse_accepts_headless_role_overrides_and_normalizes_none() {
        let cli = try_parse_hermetic(&[
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
        let cli = try_parse_hermetic(&[
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
        let error = try_parse_hermetic(&["mj", "--model", "gpt-test"])
            .expect_err("--model must require --print");
        assert!(error.to_string().contains("--print"), "{error}");
    }

    #[test]
    fn parse_model_override_splits_trailing_effort() {
        assert_eq!(
            parse_model_override("gpt-5-6-sol+high"),
            Ok(("gpt-5-6-sol".to_string(), Some("high".to_string())))
        );
        assert_eq!(
            parse_model_override("gpt-5.6-sol+high"),
            Ok(("gpt-5.6-sol".to_string(), Some("high".to_string())))
        );
    }

    #[test]
    fn parse_model_override_leaves_effort_less_selectors_unchanged() {
        assert_eq!(
            parse_model_override("deepseek-v4-pro"),
            Ok(("deepseek-v4-pro".to_string(), None))
        );
    }

    #[test]
    fn parse_model_override_still_rejects_disabled_and_auto() {
        assert!(parse_model_override("disabled").is_err());
        assert!(parse_model_override("none").is_err());
        assert!(parse_model_override("auto").is_err());
    }

    #[test]
    fn role_override_parsers_reject_empty_models() {
        for value in ["", "  ", "+high", "  +high"] {
            assert!(parse_model_override(value).is_err(), "accepted {value:?}");
            assert!(
                parse_optional_role_override(value).is_err(),
                "accepted {value:?}"
            );
        }
        assert!(parse_optional_role_override("auto").is_err());
    }

    #[test]
    fn parse_optional_role_override_splits_trailing_effort() {
        assert_eq!(
            parse_optional_role_override("gpt-5-6-terra+medium"),
            Ok(("gpt-5-6-terra".to_string(), Some("medium".to_string())))
        );
    }

    #[test]
    fn parse_optional_role_override_plus_none_maps_to_off_effort_not_disabled() {
        assert_eq!(
            parse_optional_role_override("some-model+none"),
            Ok(("some-model".to_string(), Some("off".to_string())))
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
            parse_optional_role_override("deepseek-v4-pro"),
            Ok(("deepseek-v4-pro".to_string(), None))
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
                try_parse_hermetic(&["mj", "--print", "hello", "--model", value]).is_err(),
                "accepted invalid --model override {value}"
            );
            assert!(
                try_parse_hermetic(&["mj", "--print", "hello", "--review-model", value]).is_err(),
                "accepted invalid --review-model override {value}"
            );
        }
    }

    #[test]
    fn parse_accepts_filesystem_text_limit() {
        let cli = try_parse_hermetic(&["mj", "--fs-max-text-bytes", "4096"]).expect("parse");
        assert_eq!(cli.fs_max_text_bytes, 4096);

        let cli = try_parse_hermetic(&[
            "mj",
            "--fs-max-text-bytes",
            &acp::MAX_CONFIGURABLE_FS_TEXT_BYTES.to_string(),
        ])
        .expect("parse max");
        assert_eq!(cli.fs_max_text_bytes, acp::MAX_CONFIGURABLE_FS_TEXT_BYTES);

        let cli = try_parse_hermetic(&["mj", "server", "--fs-max-text-bytes", "8192"])
            .expect("parse server");
        assert_eq!(cli.fs_max_text_bytes, 8192);
    }

    #[test]
    fn parse_rejects_unsafe_filesystem_text_limit() {
        let err = try_parse_hermetic(&["mj", "--fs-max-text-bytes", "0"]).expect_err("reject 0");
        assert!(
            err.to_string()
                .contains("filesystem text byte limit must be between 1")
        );

        let too_large = (acp::MAX_CONFIGURABLE_FS_TEXT_BYTES + 1).to_string();
        let err = try_parse_hermetic(&["mj", "--fs-max-text-bytes", &too_large])
            .expect_err("reject too large");
        assert!(
            err.to_string()
                .contains("filesystem text byte limit must be between 1")
        );

        let err = try_parse_hermetic(&["mj", "--fs-max-text-bytes", "many"])
            .expect_err("reject non-number");
        assert!(
            err.to_string()
                .contains("invalid filesystem text byte limit")
        );
    }

    #[test]
    fn command_line_modes_convert_to_runtime_modes() {
        assert!(matches!(
            headless::OutputFormat::from(HeadlessOutputFormat::Text),
            headless::OutputFormat::Text
        ));
        assert!(matches!(
            headless::OutputFormat::from(HeadlessOutputFormat::Json),
            headless::OutputFormat::Json
        ));
        assert!(matches!(
            headless::OutputFormat::from(HeadlessOutputFormat::StreamJson),
            headless::OutputFormat::StreamJson
        ));

        for (input, expected_headless, expected_config) in [
            (
                HeadlessPermissionMode::Manual,
                headless::PermissionMode::Manual,
                config::PermissionPreset::Manual,
            ),
            (
                HeadlessPermissionMode::Auto,
                headless::PermissionMode::Auto,
                config::PermissionPreset::Auto,
            ),
            (
                HeadlessPermissionMode::Yolo,
                headless::PermissionMode::Yolo,
                config::PermissionPreset::Yolo,
            ),
        ] {
            let headless = headless::PermissionMode::from(input);
            assert_eq!(
                std::mem::discriminant(&headless),
                std::mem::discriminant(&expected_headless)
            );
            assert_eq!(config::PermissionPreset::from(input), expected_config);
        }

        assert_eq!(ui_mode(false), UiMode::InlineChat);
        assert_eq!(ui_mode(true), UiMode::FullscreenTui);
    }

    #[test]
    fn configured_interface_drives_the_ui_mode_with_a_cli_override() {
        let mut cfg = Config::default();
        assert_eq!(effective_ui_mode(false, &cfg), UiMode::InlineChat);
        assert_eq!(effective_ui_mode(true, &cfg), UiMode::FullscreenTui);

        cfg.interface = config::InterfaceMode::Fullscreen;
        assert_eq!(effective_ui_mode(false, &cfg), UiMode::FullscreenTui);
        assert_eq!(effective_ui_mode(true, &cfg), UiMode::FullscreenTui);
    }

    #[test]
    fn parse_accepts_worktree_short_flag() {
        let cli = try_parse_hermetic(&["mj", "-w"]).expect("parse");
        assert_eq!(cli.worktree, Some(String::new()));

        let cli = try_parse_hermetic(&["mj", "-w", "named-tree"]).expect("parse");
        assert_eq!(cli.worktree.as_deref(), Some("named-tree"));
    }

    #[test]
    fn parse_accepts_fullscreen_tui_flags() {
        let cli = try_parse_hermetic(&["mj", "--fullscreen-tui"]).expect("parse");
        assert!(cli.fullscreen_tui);

        let cli = try_parse_hermetic(&["mj", "resume", "sess-123", "--fullscreen-tui"])
            .expect("parse resume");
        if let Some(Commands::Resume(args)) = cli.command {
            assert!(args.fullscreen_tui);
        } else {
            panic!("expected Resume subcommand");
        }

        let cli = try_parse_hermetic(&["mj", "--fullscreen-tui", "resume", "sess-123"])
            .expect("parse top-level resume");
        assert!(cli.fullscreen_tui);
    }

    /// Parse argv with every env-backed default detached. Tests must use this
    /// instead of `Cli::try_parse_from` so they stay hermetic when the test
    /// process inherits variables like `MJOLNIR_NO_UPDATE_CHECK` or
    /// `BROKK_TUI_AGENT_STDERR` from the developer's shell. (An exported
    /// `MJOLNIR_NO_UPDATE_CHECK=1` even fails *unrelated* parses outright,
    /// because "1" is not a valid clap boolean.)
    fn try_parse_hermetic(args: &[&str]) -> Result<Cli, clap::Error> {
        fn detach_env(cmd: clap::Command) -> clap::Command {
            let subcommands: Vec<String> = cmd
                .get_subcommands()
                .map(|sc| sc.get_name().to_string())
                .collect();
            let mut cmd = cmd.mut_args(|arg| arg.env(None::<&str>));
            for name in subcommands {
                cmd = cmd.mut_subcommand(name, detach_env);
            }
            cmd
        }
        use clap::FromArgMatches;
        let matches = detach_env(Cli::command()).try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn startup_update_check_runs_only_for_interactive_modes() {
        let cli = try_parse_hermetic(&["mj"]).expect("parse");
        assert!(should_run_startup_update_check(&cli));

        let cli = try_parse_hermetic(&["mj", "--no-update-check"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = try_parse_hermetic(&["mj", "--print", "hi"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = try_parse_hermetic(&["mj", "resume", "--list"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = try_parse_hermetic(&["mj", "resume", "sess-123"]).expect("parse");
        assert!(should_run_startup_update_check(&cli));

        let cli = try_parse_hermetic(&["mj", "server"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));

        let cli = try_parse_hermetic(&["mj", "mcp-bridge", "--addr", "127.0.0.1:12345"])
            .expect("parse hidden MCP bridge");
        assert!(!should_run_startup_update_check(&cli));
        let Some(Commands::McpBridge(args)) = cli.command else {
            panic!("expected MCP bridge subcommand");
        };
        assert_eq!(args.addr, "127.0.0.1:12345");

        let cli = try_parse_hermetic(&["mj", "models", "refresh"]).expect("parse");
        assert!(!should_run_startup_update_check(&cli));
    }

    #[test]
    fn parse_accepts_permission_mode_canonical_and_legacy_values() {
        let canonical =
            try_parse_hermetic(&["mj", "--permission-mode", "auto"]).expect("parse canonical");
        assert!(matches!(
            canonical.permission_mode,
            Some(HeadlessPermissionMode::Auto)
        ));

        let legacy =
            try_parse_hermetic(&["mj", "--permission-mode", "acceptEdits"]).expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            Some(HeadlessPermissionMode::Auto)
        ));

        let canonical =
            try_parse_hermetic(&["mj", "--permission-mode", "yolo"]).expect("parse canonical");
        assert!(matches!(
            canonical.permission_mode,
            Some(HeadlessPermissionMode::Yolo)
        ));

        let legacy = try_parse_hermetic(&["mj", "--permission-mode", "bypassPermissions"])
            .expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            Some(HeadlessPermissionMode::Yolo)
        ));

        let legacy =
            try_parse_hermetic(&["mj", "--permission-mode", "default"]).expect("parse legacy");
        assert!(matches!(
            legacy.permission_mode,
            Some(HeadlessPermissionMode::Manual)
        ));
    }

    #[test]
    fn parse_leaves_permission_mode_unset_when_omitted() {
        let cli = try_parse_hermetic(&["mj"]).expect("parse");
        assert!(cli.permission_mode.is_none());
    }

    #[test]
    fn parse_rejects_unknown_permission_mode_value() {
        let err = try_parse_hermetic(&["mj", "--permission-mode", "unsafe"]).expect_err("reject");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn parse_accepts_resume_session() {
        let cli = try_parse_hermetic(&["mj", "--print", "hi", "--resume-session", "sess-123"])
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
        let cli = try_parse_hermetic(&["mj", "resume"]).expect("parse");
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
        let cli = try_parse_hermetic(&["mj", "models", "refresh"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Models(ModelsArgs {
                command: ModelsCommand::Refresh
            }))
        ));

        // Assert the behavior (missing subcommand is an error naming `refresh`)
        // rather than the exact clap error kind, which changed within the 4.x
        // range the manifest accepts.
        let error = try_parse_hermetic(&["mj", "models"]).expect_err("refresh is required");
        let rendered = error.to_string();
        assert!(
            rendered.contains("refresh"),
            "error should name the missing subcommand: {rendered}"
        );
    }

    #[test]
    fn launched_adapter_kind_follows_the_launched_agent_not_roster_primary() {
        let mut codex = test_roster_agent("gpt-5", "codex-acp");
        codex.launch.kind = roster::AdapterKind::Codex;
        let mut claude = test_roster_agent("opus", "claude-acp");
        claude.launch.kind = roster::AdapterKind::Claude;
        let codex_roster = test_roster(codex.clone(), vec![codex.clone(), claude.clone()]);

        // Normal path: the launched agent is the roster primary.
        assert_eq!(
            launched_adapter_kind(&codex_roster, &selected_agent_for_role(&codex)),
            Some(roster::AdapterKind::Codex)
        );
        // Cross-adapter switch: a Claude session resumed under a
        // Codex-primary roster must not count as Codex.
        assert_eq!(
            launched_adapter_kind(&codex_roster, &selected_agent_for_role(&claude)),
            Some(roster::AdapterKind::Claude)
        );
        // The reverse: Codex resumed under a Claude-primary roster is Codex.
        let claude_roster = test_roster(claude.clone(), vec![claude.clone(), codex.clone()]);
        assert_eq!(
            launched_adapter_kind(&claude_roster, &selected_agent_for_role(&codex)),
            Some(roster::AdapterKind::Codex)
        );
        // A launch the roster does not know stays ungated.
        let unknown = SelectedAgent {
            source_id: "custom:mystery".to_string(),
            program: PathBuf::from("/usr/bin/mystery"),
            args: Vec::new(),
            env: Default::default(),
        };
        assert_eq!(launched_adapter_kind(&claude_roster, &unknown), None);
    }

    #[test]
    fn parse_memory_subcommands() {
        let cli = try_parse_hermetic(&["mj", "memory", "list"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Memory(MemoryArgs {
                command: MemoryCommand::List
            }))
        ));

        let cli = try_parse_hermetic(&["mj", "memory", "add", "--global", "prefers", "rebase"])
            .expect("parse");
        match cli.command {
            Some(Commands::Memory(MemoryArgs {
                command: MemoryCommand::Add(args),
            })) => {
                assert!(args.global);
                assert_eq!(args.text.join(" "), "prefers rebase");
            }
            other => panic!("unexpected parse: {other:?}"),
        }

        let cli = try_parse_hermetic(&["mj", "memory", "forget", "m7"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Memory(MemoryArgs {
                command: MemoryCommand::Forget(MemoryForgetArgs { ref id })
            })) if id == "m7"
        ));

        let cli = try_parse_hermetic(&["mj", "memory", "clear", "--yes"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Memory(MemoryArgs {
                command: MemoryCommand::Clear(MemoryClearArgs { yes: true })
            }))
        ));

        // `add` without text and bare `memory` are parse errors.
        try_parse_hermetic(&["mj", "memory", "add"]).expect_err("text is required");
        try_parse_hermetic(&["mj", "memory"]).expect_err("subcommand is required");
    }

    #[test]
    fn parse_agents_install_subcommand() {
        let cli = try_parse_hermetic(&["mj", "agents", "install", "--yes"]).expect("parse");
        assert!(matches!(
            cli.command,
            Some(Commands::Agents(AgentsArgs {
                command: AgentsCommand::Install(AgentsInstallArgs { yes: true })
            }))
        ));

        // Assert the behavior (missing subcommand is an error naming `install`)
        // rather than the exact clap error kind, which changed within the 4.x
        // range the manifest accepts.
        let error = try_parse_hermetic(&["mj", "agents"]).expect_err("install is required");
        let rendered = error.to_string();
        assert!(
            rendered.contains("install"),
            "error should name the missing subcommand: {rendered}"
        );
    }

    #[test]
    fn parse_server_subcommand() {
        let cli = try_parse_hermetic(&["mj", "server"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert!(args.hostname.is_none());
                assert!(!args.tailscale);
                assert!(!args.no_tailscale_detect, "detection is on by default");
                assert_eq!(args.session_ttl_days, 30);
                assert!(!args.logout_all);
                assert_eq!(args.port, remote::DEFAULT_REMOTE_CONTROL_PORT);
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
    #[test]
    fn parse_app_subcommand() {
        let cli = try_parse_hermetic(&["mj", "app"]).expect("parse app");
        assert!(matches!(
            cli.command,
            Some(Commands::App(AppArgs { history_days: 30 }))
        ));
        let cli =
            try_parse_hermetic(&["mj", "app", "--history-days", "0"]).expect("parse app history");
        assert!(matches!(
            cli.command,
            Some(Commands::App(AppArgs { history_days: 0 }))
        ));
    }

    #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
    #[tokio::test]
    async fn app_rejects_invalid_workspace_before_starting_a_desktop_shell() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let error = run_desktop_app(
            AppArgs { history_days: 30 },
            cwd.path().to_path_buf(),
            vec![PathBuf::from("relative")],
            Vec::new(),
            acp::DEFAULT_FS_TEXT_BYTES,
            CancellationToken::new(),
        )
        .await
        .expect_err("relative workspace roots must be rejected");

        assert!(
            format!("{error:#}").contains("additional workspace directory must be absolute"),
            "app must reject the additional workspace root before desktop startup: {error:#}"
        );
    }

    #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
    #[test]
    fn desktop_session_manager_keeps_setup_pending_reason() {
        use remote::ServerSessionManager;

        let cwd = tempfile::tempdir().expect("tempdir");
        let manager = desktop_session_manager(
            &Err(remote::SetupPending("no model is launchable".to_string())),
            None,
            cwd.path(),
            &[],
            &[],
            acp::DEFAULT_FS_TEXT_BYTES,
        );

        assert_eq!(manager.resolve_cwd().as_deref(), Some(cwd.path()));
        let launch_id = manager.start_session(cwd.path().to_path_buf());
        assert!(matches!(
            manager.launch_state(launch_id),
            Some(remote::ServerSessionLaunchState::Failed { error }) if error == "no model is launchable"
        ));
    }

    #[cfg(all(feature = "desktop-app", not(target_os = "android")))]
    #[test]
    fn desktop_session_manager_binds_a_resolved_roster() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let primary = test_roster_agent("test-model", "test-agent");
        let manager = desktop_session_manager(
            &Ok(test_roster(primary.clone(), vec![primary])),
            None,
            cwd.path(),
            &[],
            &[],
            acp::DEFAULT_FS_TEXT_BYTES,
        );

        assert!(manager.is_bound());
    }

    #[test]
    fn parse_server_subcommand_with_port() {
        let cli = try_parse_hermetic(&["mj", "server", "--port", "9443"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => assert_eq!(args.port, 9443),
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_server_rejects_out_of_range_ports() {
        for port in ["0", "70000"] {
            let error = try_parse_hermetic(&["mj", "server", "--port", port])
                .expect_err("port out of range");
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        }
    }

    #[test]
    fn parse_server_subcommand_with_session_flags() {
        let cli = try_parse_hermetic(&["mj", "server", "--session-ttl-days", "7", "--logout-all"])
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
        let cli = try_parse_hermetic(&["mj", "--cwd", "/tmp/test", "server"]).expect("parse");
        assert_eq!(cli.cwd, Some(PathBuf::from("/tmp/test")));
        assert!(matches!(cli.command, Some(Commands::Server(_))));
    }

    #[test]
    fn parse_server_subcommand_with_hostname() {
        let cli =
            try_parse_hermetic(&["mj", "server", "--hostname", "example.com"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert_eq!(args.hostname.as_deref(), Some("example.com"))
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_server_still_accepts_the_deprecated_tailscale_flag() {
        let cli = try_parse_hermetic(&["mj", "server", "--tailscale"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert!(args.tailscale);
                assert!(
                    !args.no_tailscale_detect,
                    "the deprecated flag must not disable detection"
                );
                assert!(args.hostname.is_none());
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    /// The flag is a no-op now, so pairing it with --hostname is no longer a
    /// conflict: the hostname simply wins, as it would without the flag.
    #[test]
    fn parse_server_accepts_the_deprecated_tailscale_flag_with_hostname() {
        let cli = try_parse_hermetic(&["mj", "server", "--tailscale", "--hostname", "example.com"])
            .expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert!(args.tailscale);
                assert_eq!(args.hostname.as_deref(), Some("example.com"));
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_server_subcommand_with_no_tailscale_detect() {
        let cli = try_parse_hermetic(&["mj", "server", "--no-tailscale-detect"]).expect("parse");
        match cli.command {
            Some(Commands::Server(args)) => {
                assert!(args.no_tailscale_detect);
                assert!(!args.tailscale);
            }
            _ => panic!("expected Server subcommand"),
        }
    }

    #[test]
    fn parse_resume_subcommand_with_session_id() {
        let cli = try_parse_hermetic(&["mj", "resume", "sess-123"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.session_id, Some("sess-123".to_string()));
            assert!(!args.list);
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_with_list_flag() {
        let cli = try_parse_hermetic(&["mj", "resume", "--list"]).expect("parse");
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
            try_parse_hermetic(&["mj", "resume", "--list", "--format", "json"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert!(args.list);
            assert!(matches!(args.format, HeadlessOutputFormat::Json));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_with_cwd() {
        let cli = try_parse_hermetic(&["mj", "resume", "--cwd", "/tmp/test"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.cwd, Some(PathBuf::from("/tmp/test")));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_additional_directories_for_new_and_resume_sessions() {
        let cli = try_parse_hermetic(&[
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

        let cli = try_parse_hermetic(&[
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

        let cli = try_parse_hermetic(&["mj", "--add-dir", "/tmp/top", "resume", "sess-123"])
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
    fn snapshot_exclusions_are_sorted_and_deduplicated() {
        assert!(configured_snapshot_exclusions(None, None).is_empty());
        assert_eq!(
            configured_snapshot_exclusions(
                Some(Path::new("/tmp/z-debug.log")),
                Some(Path::new("/tmp/a-agent.log")),
            ),
            vec![
                PathBuf::from("/tmp/a-agent.log"),
                PathBuf::from("/tmp/z-debug.log"),
            ]
        );
        assert_eq!(
            configured_snapshot_exclusions(
                Some(Path::new("/tmp/shared.log")),
                Some(Path::new("/tmp/shared.log")),
            ),
            vec![PathBuf::from("/tmp/shared.log")]
        );
    }

    #[test]
    fn literal_and_streamed_headless_prompts_are_preserved() {
        assert_eq!(
            read_headless_prompt("literal prompt".to_string()).expect("literal"),
            "literal prompt"
        );

        let mut input = &b"prompt from stdin\nwith a second line"[..];
        let mut prompt = String::new();
        read_headless_prompt_from(&mut input, &mut prompt).expect("stream prompt");
        assert_eq!(prompt, "prompt from stdin\nwith a second line");
    }

    #[test]
    fn streamed_headless_prompt_reports_read_errors() {
        struct BrokenReader;

        impl std::io::Read for BrokenReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken input"))
            }
        }

        let error = read_headless_prompt_from(&mut BrokenReader, &mut String::new())
            .expect_err("read must fail");
        assert!(error.to_string().contains("read prompt from stdin"));
    }

    #[test]
    fn worktree_helpers_preserve_plain_cwd_and_label_opened_worktree() {
        let cwd = PathBuf::from("/tmp/project");
        assert_eq!(
            prepare_worktree_for_arg(cwd.clone(), None).expect("plain cwd"),
            (cwd, None)
        );
        assert!(handle_worktree_after_tui(None, Some(UiMode::InlineChat)));

        let worktree = CreatedWorktree {
            project_root: PathBuf::from("/tmp/project"),
            worktree_root: PathBuf::from("/tmp/project/.mjolnir/worktrees/test-tree"),
            session_cwd: PathBuf::from("/tmp/project/.mjolnir/worktrees/test-tree/src"),
            was_created: false,
        };
        assert_eq!(
            worktree_label(Some(&worktree)),
            Some(mj_core::paths::folder_label(&worktree.worktree_root))
        );
        assert_eq!(worktree_label(None), None);
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
        let cli = try_parse_hermetic(&["mj", "resume", "sess-123", "--worktree", "named-tree"])
            .expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.session_id, Some("sess-123".to_string()));
            assert_eq!(args.worktree.as_deref(), Some("named-tree"));
        } else {
            panic!("expected Resume subcommand");
        }

        let cli = try_parse_hermetic(&["mj", "resume", "sess-123", "--worktree"])
            .expect("parse missing value");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.worktree.as_deref(), Some(""));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_rejects_list_with_session_id() {
        let err = try_parse_hermetic(&["mj", "resume", "sess-123", "--list"]).expect_err("reject");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parse_resume_subcommand_rejects_format_without_list() {
        let err = try_parse_hermetic(&["mj", "resume", "--format", "json"]).expect_err("reject");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parse_resume_subcommand_with_agent_stderr() {
        let cli =
            try_parse_hermetic(&["mj", "resume", "--agent-stderr", "agent.log"]).expect("parse");
        if let Some(Commands::Resume(args)) = cli.command {
            assert_eq!(args.agent_stderr, Some(PathBuf::from("agent.log")));
        } else {
            panic!("expected Resume subcommand");
        }
    }

    #[test]
    fn parse_resume_subcommand_combined_flags() {
        let cli = try_parse_hermetic(&[
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
    fn codex_side_role_isolates_config_but_shares_live_login_state() {
        let source = tempfile::tempdir().expect("source home");
        std::fs::write(source.path().join("auth.json"), "auth").expect("auth");
        std::fs::write(source.path().join("config.toml"), "config").expect("config");
        let mut role = test_roster_agent("codex-model", "codex-acp");
        role.launch.kind = roster::AdapterKind::Codex;

        let (prepared, guard) =
            isolated_subagent_role_from_home(role, "review", source.path()).expect("isolate");
        let guard = guard.expect("isolated home guard");
        let isolated_home =
            PathBuf::from(prepared.launch.env.get("CODEX_HOME").expect("CODEX_HOME"));
        assert_eq!(isolated_home, guard.path());
        assert_eq!(
            std::fs::read_to_string(isolated_home.join("auth.json")).expect("shared auth"),
            "auth"
        );
        assert_eq!(
            std::fs::read_to_string(isolated_home.join("config.toml")).expect("copied config"),
            "config"
        );
        // Config edits must stay private to the seat...
        std::fs::write(isolated_home.join("config.toml"), "seat config").expect("seat config");
        assert_eq!(
            std::fs::read_to_string(source.path().join("config.toml")).expect("source config"),
            "config"
        );
        #[cfg(unix)]
        {
            // ...but a re-login that rewrites the real auth.json in place must
            // reach the running seat, and a token the seat's codex refreshes
            // must land in the real home rather than dying with the temp dir.
            std::fs::write(source.path().join("auth.json"), "relogin").expect("relogin");
            assert_eq!(
                std::fs::read_to_string(isolated_home.join("auth.json")).expect("shared auth"),
                "relogin"
            );
            std::fs::write(isolated_home.join("auth.json"), "rotated").expect("rotate");
            assert_eq!(
                std::fs::read_to_string(source.path().join("auth.json")).expect("source auth"),
                "rotated"
            );
        }
    }

    #[test]
    fn codex_side_role_requires_login_but_other_adapters_need_no_isolation() {
        let source = tempfile::tempdir().expect("source home");
        let mut codex = test_roster_agent("codex-model", "codex-acp");
        codex.launch.kind = roster::AdapterKind::Codex;
        let error = isolated_subagent_role_from_home(codex, "review", source.path())
            .expect_err("missing auth must fail");
        assert!(error.to_string().contains("has no auth.json"));

        let custom = test_roster_agent("custom-model", "custom");
        let (prepared, guard) = isolated_subagent_role(custom.clone(), "review").expect("custom");
        assert_eq!(prepared.model.model, custom.model.model);
        assert!(guard.is_none());

        let (roles, guard) = isolated_subagent_roles(vec![custom], "review").expect("custom roles");
        assert_eq!(roles.len(), 1);
        assert!(guard.is_none());
    }

    #[tokio::test]
    async fn startup_helpers_complete_their_future_and_accept_no_loading_task() {
        stop_new_session_loading(None).await;
        let value = with_startup_spinner(async { Ok::<_, anyhow::Error>(42) })
            .await
            .expect("future result");
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn mcp_bridge_serves_an_initialized_root_session() {
        use agent_client_protocol::schema::v1::McpServer;
        use tokio::{
            io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
            net::TcpStream,
        };

        let temp = tempfile::tempdir().expect("memory tempdir");
        let session_memory = memory::SessionMemory {
            store_path: temp.path().join("memories.json"),
            config_path: None,
            project: temp.path().to_path_buf(),
            inject: true,
            cleanup: false,
            tools: true,
        };
        let server = memory::ToolServer::start(&session_memory)
            .await
            .expect("start bridge");
        let McpServer::Stdio(stdio) = server.advertised() else {
            panic!("bridge must advertise stdio");
        };
        let addr = stdio
            .args
            .iter()
            .skip_while(|arg| arg.as_str() != "--addr")
            .nth(1)
            .expect("bridge address");
        let token = &stdio
            .env
            .iter()
            .find(|variable| variable.name == mj_core::mcp_bridge::TOKEN_ENV)
            .expect("bridge token")
            .value;
        let stream = TcpStream::connect(addr).await.expect("connect bridge");
        let (read, mut write) = stream.into_split();
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "root-fixture", "version": "1"}
            }
        });
        write
            .write_all(format!("{token}\n{initialize}\n").as_bytes())
            .await
            .expect("initialize bridge");
        let response = BufReader::new(read)
            .lines()
            .next_line()
            .await
            .expect("read response")
            .expect("bridge remains open");
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("response is JSON");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["serverInfo"]["name"], "mj-memory");
    }

    #[tokio::test]
    async fn inline_session_load_reports_closed_command_channel() {
        let (commands, receiver) = mpsc::unbounded_channel();
        drop(receiver);

        assert_eq!(
            request_inline_session_load(
                &commands,
                "session".to_string(),
                PathBuf::from("/tmp/project"),
                None,
            )
            .await,
            LoadSessionResult::Fallback {
                message: "ACP runtime command channel closed".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn inline_session_load_forwards_request_and_response() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let request = tokio::spawn(async move {
            request_inline_session_load(
                &commands,
                "session".to_string(),
                PathBuf::from("/tmp/project"),
                Some("Title".to_string()),
            )
            .await
        });

        let command = receiver.recv().await.expect("load command");
        match command {
            UiCommand::LoadSession {
                session_id,
                cwd,
                title,
                responder,
            } => {
                assert_eq!(session_id, "session");
                assert_eq!(cwd, PathBuf::from("/tmp/project"));
                assert_eq!(title.as_deref(), Some("Title"));
                responder
                    .send(LoadSessionResult::Switched)
                    .expect("response accepted");
            }
            _ => panic!("expected load session command"),
        }

        assert_eq!(
            request.await.expect("request task"),
            LoadSessionResult::Switched
        );
    }

    #[tokio::test]
    async fn inline_session_load_handles_dropped_response_and_timeout() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let closed = tokio::spawn(async move {
            request_inline_session_load(
                &commands,
                "closed".to_string(),
                PathBuf::from("/tmp/project"),
                None,
            )
            .await
        });
        drop(receiver.recv().await.expect("closed command"));
        assert_eq!(
            closed.await.expect("closed task"),
            LoadSessionResult::Fallback {
                message: "ACP runtime closed before session switch completed".to_string(),
            }
        );

        let (commands, mut receiver) = mpsc::unbounded_channel();
        let timed_out = tokio::spawn(async move {
            request_inline_session_load_with_timeout(
                &commands,
                "slow".to_string(),
                PathBuf::from("/tmp/project"),
                None,
                Duration::from_millis(10),
            )
            .await
        });
        let pending_command = receiver.recv().await.expect("slow command");
        assert_eq!(
            timed_out.await.expect("timeout task"),
            LoadSessionResult::Fallback {
                message: "ACP runtime did not complete session switch within 15s".to_string(),
            }
        );
        drop(pending_command);
    }

    #[tokio::test]
    async fn task_waiter_handles_success_failure_and_timeout() {
        wait_for_task("success", tokio::spawn(async {})).await;
        wait_for_task_with_timeout(
            "panic",
            tokio::spawn(async { panic!("task panic") }),
            Duration::from_millis(50),
        )
        .await;
        wait_for_task_with_timeout(
            "slow",
            tokio::spawn(std::future::pending()),
            Duration::from_millis(10),
        )
        .await;
    }

    #[test]
    fn logging_without_a_path_is_disabled_and_bad_parent_is_reported() {
        init_logging(None).expect("disabled logging");

        let temp = tempfile::tempdir().expect("tempdir");
        let file_parent = temp.path().join("not-a-directory");
        std::fs::write(&file_parent, "file").expect("parent file");
        let error = init_logging(Some(&file_parent.join("debug.log")))
            .expect_err("file parent cannot become directory");
        assert!(error.to_string().contains("create log dir"));
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

    #[test]
    fn unavailable_review_fanout_keeps_the_resolver_error_verbatim() {
        let error = review_fanout_error(
            false,
            false,
            "auto",
            true,
            &[
                "subagent delegation is disabled: no launchable subagent model is available. Authenticate Claude ACP."
                    .to_string(),
                "agentic review supervisor is disabled: no distinct launchable review model is available. Authenticate Claude ACP."
                    .to_string(),
                "claude-acp unavailable: authentication expired".to_string(),
            ],
        );

        assert!(error.contains("claude-acp unavailable: authentication expired"));
        assert!(error.contains("subagent delegation is disabled"));
        assert!(error.contains("review supervisor is disabled"));
    }
}

//! mjolnir: an interactive terminal client for any ACP-speaking agent.
//!
//! Picks an agent (from the ACP registry, the bundled `anvil` default, or
//! a Custom command) the first time it runs, persists the choice to
//! `~/.config/mj/config.toml`, then spawns the agent as a child process
//! and renders the session in a ratatui chat UI.

mod acp;
mod app;
mod clipboard;
mod config;
mod event;
mod headless;
mod install;
mod picker;
mod registry;
mod ui;
mod worktree;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use tokio::sync::mpsc;

use crate::app::UiExitReason;
use crate::config::{Config, SelectedAgent, history_path};
use crate::picker::PickerOutcome;
use crate::worktree::CreatedWorktree;

#[derive(Debug, Parser)]
#[command(name = "mj", version, about = "Interactive ACP chat TUI")]
struct Cli {
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

    /// Resume an existing ACP session in headless mode instead of
    /// opening a new one.
    #[arg(long)]
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_file.as_deref())?;

    let cwd = match cli.cwd {
        Some(p) => p,
        None => std::env::current_dir().context("current dir")?,
    };

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

    let (cwd, worktree) = match cli.worktree.as_deref() {
        None => (cwd, None),
        Some("") => {
            // `--worktree` with no value: create a new one.
            let created = prepare_new_worktree(&cwd)?;
            (created.session_cwd.clone(), Some(created))
        }
        Some(name_or_path) => {
            // `--worktree <name>`: reuse an existing one.
            let created = prepare_existing_worktree(&cwd, name_or_path)?;
            (created.session_cwd.clone(), Some(created))
        }
    };
    let worktree_label = worktree
        .as_ref()
        .and_then(|w| w.worktree_root.file_name())
        .map(|n| n.to_string_lossy().into_owned());

    let mut terminal = ui::setup_terminal().context("setup terminal")?;

    // Run the application; ensure the terminal is restored even on
    // error so the user's shell isn't left in alt-screen / raw mode.
    let result = run_app(&mut terminal, cwd, cli.agent_stderr, worktree_label).await;

    if let Err(e) = ui::restore_terminal(&mut terminal) {
        tracing::warn!("restore terminal failed: {e}");
    }

    // Remind the user where the worktree lives so they don't lose track
    // of their work — the alt-screen has just been torn down, so writes
    // to stdout now land in their normal scrollback.
    if let Some(w) = worktree.as_ref() {
        println!("Worktree: {}", w.worktree_root.display());
        // Offer to clean up a freshly-created worktree. Skip the prompt
        // for reused worktrees — the user explicitly asked to work in
        // an existing one, so removing it would be surprising.
        if w.was_created {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            if let Err(e) = worktree::prompt_remove_on_exit(w, &mut input, &mut output) {
                tracing::warn!("worktree cleanup prompt failed: {e:#}");
            }
        }
    }

    result
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

async fn run_app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cwd: PathBuf,
    agent_stderr: Option<PathBuf>,
    worktree_label: Option<String>,
) -> Result<()> {
    let config_path = config::default_config_path();
    let mut cfg = Config::load(&config_path)?;

    // Supervisor loop. We start a session for the currently-configured
    // agent; if the user invokes `/mj:agents`, the UI exits with
    // `SwapAgent`, we run the picker again, persist, and loop.
    let mut last_source_id: Option<String> = None;
    loop {
        let agent = match cfg.agent.clone() {
            Some(a) => {
                last_source_id = Some(a.source_id.clone());
                a
            }
            None => {
                let outcome = run_picker_with_registry(terminal, last_source_id.clone()).await?;
                let Some(outcome) = outcome else {
                    return Ok(());
                };
                let selected = picker_outcome_to_selected(outcome);
                cfg.agent = Some(selected.clone());
                cfg.save(&config_path)
                    .with_context(|| format!("save {}", config_path.display()))?;
                last_source_id = Some(selected.source_id.clone());
                selected
            }
        };

        let reason = run_session(
            terminal,
            &agent,
            cwd.clone(),
            agent_stderr.clone(),
            worktree_label.clone(),
        )
        .await?;
        match reason {
            UiExitReason::Quit => return Ok(()),
            UiExitReason::SwapAgent => {
                // Drop the current agent; the next loop iteration runs
                // the picker so the user can pick a new one. We persist
                // only when they actually commit a choice.
                cfg.agent = None;
                continue;
            }
        }
    }
}

async fn run_picker_with_registry(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    current_source_id: Option<String>,
) -> Result<Option<PickerOutcome>> {
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
        current_source_id,
    )
    .await
}

fn picker_outcome_to_selected(o: PickerOutcome) -> SelectedAgent {
    SelectedAgent {
        source_id: o.source_id,
        program: o.program,
        args: o.args,
        env: o.env,
    }
}

async fn run_session(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<PathBuf>,
    worktree_label: Option<String>,
) -> Result<UiExitReason> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let runtime_cfg = acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd,
        resume_session: None,
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
    let ui_result = ui::run(terminal, cmd_tx, event_rx, worktree_label, Some(&hist_path)).await;

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

    ui_result
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
        assert!(help.contains("[possible values: default, acceptEdits, bypassPermissions]"));
        assert!(!help.contains("accept-edits"));
        assert!(!help.contains("bypass-permissions"));
    }
}

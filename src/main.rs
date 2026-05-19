//! mjolnir: an interactive terminal client for any ACP-speaking agent.
//!
//! Picks an agent (from the ACP registry, the bundled `anvil` default, or
//! a Custom command) the first time it runs, persists the choice to
//! `~/.config/mj/config.toml`, then spawns the agent as a child process
//! and renders the session in a ratatui chat UI.

mod acp;
mod app;
mod config;
mod event;
mod install;
mod picker;
mod registry;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::mpsc;

use crate::app::UiExitReason;
use crate::config::{Config, SelectedAgent};
use crate::picker::PickerOutcome;

#[derive(Parser)]
#[command(name = "mj", version, about = "Interactive ACP chat TUI")]
struct Cli {
    /// Working directory used when opening a new session. Defaults to
    /// the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Path to a log file. When unset, logging is disabled because the
    /// TUI owns the terminal and stderr would corrupt the screen.
    #[arg(long, env = "BROKK_TUI_LOG")]
    log_file: Option<PathBuf>,

    /// Capture the agent subprocess's stderr to this file. When unset
    /// the agent's stderr is discarded via `Stdio::null()` (/dev/null on
    /// Unix, NUL on Windows) so it doesn't scribble over the TUI.
    #[arg(long, env = "BROKK_TUI_AGENT_STDERR")]
    agent_stderr: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_file.as_deref())?;

    let cwd = match cli.cwd {
        Some(p) => p,
        None => std::env::current_dir().context("current dir")?,
    };

    let mut terminal = ui::setup_terminal().context("setup terminal")?;

    // Run the application; ensure the terminal is restored even on
    // error so the user's shell isn't left in alt-screen / raw mode.
    let result = run_app(&mut terminal, cwd, cli.agent_stderr).await;

    if let Err(e) = ui::restore_terminal(&mut terminal) {
        tracing::warn!("restore terminal failed: {e}");
    }
    result
}

async fn run_app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cwd: PathBuf,
    agent_stderr: Option<PathBuf>,
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

        let reason = run_session(terminal, &agent, cwd.clone(), agent_stderr.clone()).await?;
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
) -> Result<UiExitReason> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let runtime_cfg = acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd,
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

    let ui_result = ui::run(terminal, cmd_tx, event_rx).await;

    // UI exited. `cmd_tx` was moved into `ui::run` and is now dropped,
    // which causes `drive_session` to see `None` on its `recv()` and
    // return, after which `acp::run` calls `child.kill().await` and
    // cleans up. Wait for that natural shutdown for a bounded window so
    // the agent process is reaped, and only abort as a last resort if
    // the runtime is wedged (e.g. blocked on a `block_task().await` for
    // a never-arriving response).
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

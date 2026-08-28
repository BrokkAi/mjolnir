//! `mj-app`: the Mjolnir desktop shell.
//!
//! Runs the same remote-control server the CLI serves, then opens it in a
//! native WebView. This lives in its own binary so the WebKitGTK/WebView2
//! dependency never reaches `mj`, which has to start on headless machines.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use mj_core::acp;
use mj_core::config::{self, Config};
use mj_core::roster;
use mj_desktop as desktop;
use mj_remote as remote;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(name = "mj-app", version, about = "Mjolnir desktop viewer")]
struct Cli {
    /// Workspace directory to open. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Additional absolute workspace roots to expose to agents.
    #[arg(long = "additional-directory", value_name = "DIR")]
    additional_directories: Vec<PathBuf>,

    /// Absolute paths to keep out of workspace snapshots.
    #[arg(long = "snapshot-exclusion", value_name = "PATH")]
    snapshot_exclusions: Vec<PathBuf>,

    /// Maximum bytes read from a single text file.
    #[arg(long, default_value_t = acp::DEFAULT_FS_TEXT_BYTES)]
    fs_max_text_bytes: u64,

    /// Days of disconnected-session history to keep. Pass 0 to retain it
    /// forever.
    #[arg(long, default_value_t = 30)]
    history_days: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = match cli.cwd {
        Some(path) => path,
        None => std::env::current_dir().context("current dir")?,
    };
    let runtime = tokio::runtime::Runtime::new().context("start desktop runtime")?;
    runtime.block_on(run(
        cli.history_days,
        cwd,
        cli.additional_directories,
        cli.snapshot_exclusions,
        cli.fs_max_text_bytes,
    ))
}

async fn run(
    history_days: u32,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    snapshot_exclusions: Vec<PathBuf>,
    fs_max_text_bytes: u64,
) -> Result<()> {
    let termination = CancellationToken::new();
    tokio::spawn({
        let termination = termination.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                termination.cancel();
            }
        }
    });

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
    let session_manager = remote::remote_host::desktop_session_manager(
        &resolved,
        remote::remote_host::config_file_hash(&config_path),
        &cwd,
        workspace_roots.additional_directories(),
        &snapshot_exclusions,
        fs_max_text_bytes,
    );

    let server_stop = termination.child_token();
    let (handle, serve) = remote::prepare_desktop_server(remote::DesktopServerOptions {
        config: cfg,
        roster: resolved,
        history_days,
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

fn validate_workspace_roots(
    cwd: &Path,
    additional_directories: &[PathBuf],
) -> Result<mj_core::paths::WorkspaceRoots> {
    mj_core::paths::WorkspaceRoots::new(cwd, additional_directories)
}

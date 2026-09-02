//! `mj app` launcher and the controller side of desktop bootstrap.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use hel::hel_desktop::{DesktopLaunch, sibling_executable};
use hel::hel_server::{cookie_key_path, load_or_create_cookie_key, mint_desktop_session_cookie};

use crate::daemon::{self, WebViewerStatus};

pub(crate) async fn run_desktop_app() -> Result<()> {
    let executable = desktop_executable()?;
    let controller = std::env::current_exe().context("locate the mj executable")?;
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(&executable);
        command.env("MJ_CONTROLLER_BINARY", controller);
        let status = hel::hel_subprocess::run_inherited(&mut command)
            .with_context(|| format!("start desktop application {}", executable.display()))?;
        ensure!(
            status.success(),
            "Mjolnir desktop application exited with {status}"
        );
        Ok(())
    })
    .await
    .context("desktop launcher task panicked")?
}

/// Print a one-use desktop launch document for the sibling `mj-desktop`.
///
/// This command is hidden from help because the JSON contains a signed viewer
/// cookie. The desktop process captures it directly from this process's
/// stdout; it must never be sent through argv or the environment.
pub(crate) async fn desktop_bootstrap() -> Result<()> {
    let mut client = daemon::connect_or_start().await?;
    let status = client.status().await?;
    let viewer_url = match status.phone_status {
        WebViewerStatus::Ready { viewer_url, .. } => viewer_url,
        WebViewerStatus::Disabled => {
            bail!(
                "the web viewer is disabled; enable [phone] in config.toml and run `mj daemon restart`"
            )
        }
        WebViewerStatus::Starting => {
            bail!("the web viewer is still starting; retry in a moment or check `mj daemon status`")
        }
        WebViewerStatus::Stopped => {
            bail!("the web viewer is stopped; run `mj daemon restart`")
        }
        WebViewerStatus::Error { message } => {
            bail!("the web viewer failed to start: {message}; run `mj daemon restart`")
        }
    };

    let key = load_or_create_cookie_key(&cookie_key_path())
        .context("read the viewer cookie signing key")?;
    let bootstrap_cookie_value =
        mint_desktop_session_cookie(&key).context("mint a desktop viewer session")?;
    let launch = DesktopLaunch::new(viewer_url, bootstrap_cookie_value);
    let mut json = launch.to_json()?;
    json.push(b'\n');
    std::io::stdout()
        .lock()
        .write_all(&json)
        .context("write desktop launch")
}

fn desktop_executable() -> Result<PathBuf> {
    let path = if let Some(path) = hel::hel_config::env_override_os("DESKTOP_BINARY") {
        PathBuf::from(path)
    } else {
        let current = std::env::current_exe().context("locate the mj executable")?;
        sibling_executable(&current, "mj-desktop")
    };
    ensure!(
        path.is_file(),
        "desktop application is missing: {}; install mj-desktop beside mj or set MJ_DESKTOP_BINARY",
        path.display()
    );
    Ok(path)
}

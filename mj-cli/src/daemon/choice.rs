//! The first launch on a computer that has never run a daemon asks where the
//! daemon should run, before anything connects to one.
//!
//! The answer is the `[daemon]` placement every later launch reads, so an
//! SSH destination is only saved once a real connection to it has worked: a
//! mistyped host would otherwise fail every following launch.

use anyhow::{Context, Result};
use mj_client::daemon::ssh::{self, DiscoveryIntent};
use mj_controller::readline::{Line, LineReader};
use mj_core::config::{DaemonConfig, DaemonRuntime, config_path};

pub(crate) enum PlacementChoice {
    /// A placement is settled; start the dashboard.
    Continue,
    /// The user ended input (Ctrl-D or Ctrl-C) without choosing.
    Cancelled,
}

enum Remote {
    Saved,
    Back,
    Cancelled,
}

/// Whether no daemon has ever run on this computer: no placement is
/// configured, and there is neither a store nor an endpoint. Either means a
/// local daemon already ran, so a computer that stopped its daemon is not
/// asked again. Nothing else counts: a configuration copied to a new computer,
/// or a standalone `mj setup`, has not started a daemon there.
fn is_first_launch() -> Result<bool> {
    if !DaemonConfig::load_placement(&config_path())?.is_default() {
        return Ok(false);
    }
    Ok(!mj_controller::database::database_path().exists()
        && !mj_client::daemon::metadata_path().exists())
}

/// Ask where the daemon runs when this is the first launch, and save an SSH
/// answer. Choosing this computer saves nothing: the default placement is a
/// local daemon, and starting it creates the store that ends the first launch.
pub(crate) async fn ask_where_daemon_runs() -> Result<PlacementChoice> {
    let first = mj_core::runtime::spawn_blocking(is_first_launch)
        .await
        .context("first-launch check task failed")??;
    if !first {
        return Ok(PlacementChoice::Continue);
    }
    loop {
        eprintln!("No Mjolnir daemon has run on this computer. Where should it run?");
        eprintln!("  1) Start a daemon on this computer");
        eprintln!("  2) Connect to a daemon on another computer over SSH");
        // Ctrl-C leaves instead of taking the default: the default starts a
        // daemon and a store, which ends this prompt for good.
        let Line::Text(answer) = ask("Choose 1 or 2 [1]: ").await? else {
            return Ok(PlacementChoice::Cancelled);
        };
        match answer.as_str() {
            "" | "1" => return Ok(PlacementChoice::Continue),
            "2" => match connect_remote().await? {
                Remote::Saved => return Ok(PlacementChoice::Continue),
                Remote::Back => {}
                Remote::Cancelled => return Ok(PlacementChoice::Cancelled),
            },
            _ => eprintln!("Enter 1 or 2."),
        }
    }
}

async fn ask(label: &'static str) -> Result<Line> {
    mj_core::runtime::spawn_blocking(move || LineReader::default().read_line_or_interrupt(label))
        .await
        .context("prompt task failed")?
}

async fn connect_remote() -> Result<Remote> {
    loop {
        let host = match ask(
            "SSH destination (user@host, or a Host from your SSH config; empty to go back): ",
        )
        .await?
        {
            Line::Text(host) => host,
            Line::Interrupted => return Ok(Remote::Back),
            Line::Eof => return Ok(Remote::Cancelled),
        };
        if host.is_empty() {
            return Ok(Remote::Back);
        }
        if host.starts_with('-') {
            eprintln!("An SSH destination cannot start with '-'.");
            continue;
        }
        let placement = DaemonConfig {
            runtime: DaemonRuntime::Ssh,
            host: Some(host.clone()),
            ..DaemonConfig::default()
        };
        eprintln!("Connecting to {host}, starting its daemon if needed... (Ctrl-C to cancel)");
        if let Err(error) = try_connect(&placement).await {
            eprintln!("Could not use the daemon on {host}: {error:#}\n");
            continue;
        }
        let saved = placement.clone();
        // The file must still hold the default this prompt was shown for: a
        // second first launch that chose another host first is not replaced.
        let saving = mj_core::runtime::spawn_blocking(move || {
            saved.replace_placement(&config_path(), &DaemonConfig::default())
        })
        .await
        .context("save daemon placement task failed")?;
        match saving {
            Ok(()) => {
                eprintln!(
                    "This computer will use the daemon on {host}. To change that, edit [daemon] in {}.",
                    config_path().display()
                );
                return Ok(Remote::Saved);
            }
            // The connection worked; only the saving did not. Report it and
            // leave the menu open so local remains a choice.
            Err(error) => eprintln!("The connection works, but it could not be saved: {error:#}\n"),
        }
    }
}

/// The connection every later launch makes: discovery, the tunnel, and the
/// proof of the daemon's identity. The tunnel is released again; the remote
/// daemon keeps running. Ctrl-C cancels it and reaps the SSH process group,
/// so a server that accepts the connection and then stalls can be left.
async fn try_connect(placement: &DaemonConfig) -> Result<()> {
    let instance = placement.remote_instance();
    let cancel = async {
        // A failed handler registration must not read as a cancellation.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let mut lease = ssh::open(
        placement,
        instance.as_deref(),
        DiscoveryIntent::Start,
        cancel,
    )
    .await?;
    lease.terminate().await
}

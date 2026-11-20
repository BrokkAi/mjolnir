//! The first launch on a computer that has never run a daemon asks where the
//! daemon should run, before anything starts one.
//!
//! The answer is the `[daemon]` placement every later launch reads, so an
//! SSH destination is only saved once a real connection to it has worked: a
//! mistyped host would otherwise fail every following launch.

use anyhow::{Context, Result};
use mj_client::daemon::ssh::{self, DiscoveryIntent};
use mj_core::config::{DaemonConfig, DaemonRuntime, config_path, data_dir};

pub(crate) enum PlacementChoice {
    /// A placement is settled; start the dashboard.
    Continue,
    /// The user ended input (Ctrl-D) without choosing.
    Cancelled,
}

enum Remote {
    Saved,
    Back,
    Cancelled,
}

/// Whether this launch is the first on this computer: no placement is
/// configured, no daemon has run here or left an endpoint, and automatic
/// setup has neither started nor finished. A setup marker or a store means a
/// local daemon already ran, so a computer that stopped its daemon is not
/// asked again.
fn is_first_launch() -> Result<bool> {
    let config = config_path();
    let data = data_dir();
    if !DaemonConfig::load_placement(&config)?.is_default() {
        return Ok(false);
    }
    if mj_controller::database::database_path().exists()
        || mj_client::daemon::metadata_path().exists()
        || data.join("setup-state").exists()
    {
        return Ok(false);
    }
    mj_setup::setup::automatic_setup_due(&config, &data)
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
        let Some(answer) = ask("Choose 1 or 2 [1]: ").await? else {
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

/// `None` is end of input. Ctrl-C reads as an empty answer, which every
/// prompt here treats as its default or as going back.
async fn ask(label: &'static str) -> Result<Option<String>> {
    mj_core::runtime::spawn_blocking(move || {
        mj_controller::readline::LineReader::default().read_line(label)
    })
    .await
    .context("prompt task failed")?
}

async fn connect_remote() -> Result<Remote> {
    loop {
        let Some(host) =
            ask("SSH destination (user@host, or a Host from your SSH config; empty to go back): ")
                .await?
        else {
            return Ok(Remote::Cancelled);
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
        eprintln!("Connecting to {host}, starting its daemon if needed...");
        match try_connect(&placement).await {
            Ok(()) => {
                let saved = placement.clone();
                mj_core::runtime::spawn_blocking(move || saved.save_placement(&config_path()))
                    .await
                    .context("save daemon placement task failed")??;
                eprintln!(
                    "This computer will use the daemon on {host}. To change that, edit [daemon] in {}.",
                    config_path().display()
                );
                return Ok(Remote::Saved);
            }
            Err(error) => eprintln!("Could not use the daemon on {host}: {error:#}\n"),
        }
    }
}

/// The connection every later launch makes: discovery, the tunnel, and the
/// proof of the daemon's identity. The tunnel is released again; the remote
/// daemon keeps running.
async fn try_connect(placement: &DaemonConfig) -> Result<()> {
    let instance = placement
        .instance
        .clone()
        .or_else(mj_core::config::instance_name)
        .unwrap_or_else(|| "default".into());
    let mut lease = ssh::open(
        placement,
        &instance,
        DiscoveryIntent::Start,
        std::future::pending(),
    )
    .await?;
    lease.terminate().await
}

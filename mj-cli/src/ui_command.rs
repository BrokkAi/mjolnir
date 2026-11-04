//! `mj ui`: change what the person's dashboard shows, on their behalf.
//!
//! Each attached dashboard applies the request as if the person had pressed
//! the keys. `mj ui settings` opens Settings with the changes staged; they
//! reach the configuration only when the person presses Save.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use mj_client::ui_request::{SessionPlacement, SettingEdit, UiAction, UiPane, UiRequestBody};
use mj_core::workspace::PaneSize;

use crate::api_client::ApiClient;

#[derive(Debug, Args)]
pub(crate) struct UiArgs {
    #[command(subcommand)]
    command: UiCommand,
    /// Print the daemon's answer as JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum UiCommand {
    /// Set one support pane's size, as its title-bar control does.
    Pane {
        /// The pane to resize.
        pane: PaneArg,
        /// Its new size.
        size: SizeArg,
    },
    /// Show a session in the conversation area, or in a new pane beside it.
    Show {
        /// The session's full ID.
        session: String,
        /// Open it in a new pane to the right of the conversation.
        #[arg(long, conflicts_with = "below")]
        right: bool,
        /// Open it in a new pane below the conversation.
        #[arg(long)]
        below: bool,
    },
    /// Open Settings with these changes staged for the person to review and
    /// save. Paths are the Settings page paths, which match the
    /// configuration file's keys: `notify.bell=false`, `theme=dark`,
    /// `advanced.auto_afk_minutes=30`. A value that reads as JSON (`true`,
    /// `30`, `null`) is used as that value; anything else is text, and
    /// `null` clears a field.
    Settings {
        /// One or more settings to stage, each as PATH=VALUE.
        #[arg(required = true, value_name = "PATH=VALUE")]
        edits: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PaneArg {
    Sessions,
    Targets,
    Profiles,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SizeArg {
    Minimized,
    Standard,
    Maximized,
}

pub(crate) async fn ui(args: UiArgs) -> Result<()> {
    let action = match args.command {
        UiCommand::Pane { pane, size } => UiAction::PaneSize {
            pane: match pane {
                PaneArg::Sessions => UiPane::Sessions,
                PaneArg::Targets => UiPane::Targets,
                PaneArg::Profiles => UiPane::Profiles,
            },
            size: match size {
                SizeArg::Minimized => PaneSize::Minimized,
                SizeArg::Standard => PaneSize::Standard,
                SizeArg::Maximized => PaneSize::Maximized,
            },
        },
        UiCommand::Show {
            session,
            right,
            below,
        } => UiAction::ShowSession {
            session_id: session,
            placement: if right {
                SessionPlacement::Right
            } else if below {
                SessionPlacement::Below
            } else {
                SessionPlacement::Main
            },
        },
        UiCommand::Settings { edits } => UiAction::StageSettings {
            edits: edits
                .iter()
                .map(|edit| setting_edit(edit))
                .collect::<Result<_>>()?,
        },
    };
    let staged = match &action {
        UiAction::StageSettings { edits } => Some(edits.len()),
        _ => None,
    };
    let receipt = ApiClient::connect()
        .await?
        .request_ui(&UiRequestBody {
            from_session_id: crate::api_commands::calling_session()?,
            action,
        })
        .await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt).context("serialize the API response")?
        );
        return Ok(());
    }
    let dashboards = match receipt.dashboards {
        1 => "1 dashboard".to_owned(),
        count => format!("{count} dashboards"),
    };
    match staged {
        Some(count) => println!(
            "Staged {count} setting{} in Settings on {dashboards}. Nothing is saved until the person presses Save.",
            if count == 1 { "" } else { "s" }
        ),
        None => println!("Sent to {dashboards}."),
    }
    Ok(())
}

/// `notify.bell=false` as a staged edit.
fn setting_edit(text: &str) -> Result<SettingEdit> {
    let Some((path, value)) = text.split_once('=') else {
        bail!("{text:?} is not PATH=VALUE, for example notify.bell=false");
    };
    let path = path
        .trim()
        .split('.')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if path.iter().any(String::is_empty) {
        bail!("{text:?} names an empty setting path");
    }
    let value =
        serde_json::from_str(value).unwrap_or_else(|_| serde_json::Value::String(value.to_owned()));
    Ok(SettingEdit { path, value })
}

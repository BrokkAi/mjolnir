//! Preparation for the deliberately small `mj simple` terminal workflow.

use anyhow::{Context, Result, ensure};
use clap::Args;
use mj_core::config::{Config, TargetTemplate};
use mj_core::go::{GoPreferences, GoRecipe};
use std::path::PathBuf;

const SIMPLE_TARGET_ID: &str = "localhost";

#[derive(Debug, Args)]
pub(crate) struct SimpleArgs {
    /// Source folder (defaults to the current directory).
    pub folder: Option<PathBuf>,
}

pub(crate) fn prepare(args: SimpleArgs) -> Result<mj_tui::GoMode> {
    use std::io::IsTerminal;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "mj simple needs an interactive terminal"
    );
    let directory = args
        .folder
        .unwrap_or(std::env::current_dir().context("read current folder")?);
    let directory = directory
        .canonicalize()
        .with_context(|| format!("open folder {}", directory.display()))?;
    ensure!(
        directory.is_dir(),
        "{} is not a folder",
        directory.display()
    );

    let config = Config::load()?;
    let preferences = GoPreferences::load(&GoPreferences::path())?;
    let saved = preferences.recipe(&directory);
    let profile_id = saved
        .as_ref()
        .map(|recipe| recipe.profile_id.as_str())
        .filter(|profile_id| config.enabled_profile(profile_id).is_some())
        .or_else(|| config.enabled_profiles().next().map(|(id, _)| id))
        .context("mj simple needs an enabled agent profile; add one in Settings → Agent Profiles, or run `mj setup` and enable one first")?
        .to_owned();
    let target = config.targets.get(SIMPLE_TARGET_ID).with_context(|| {
        format!(
            "mj simple requires the built-in `{SIMPLE_TARGET_ID}` target; it is unavailable in this configuration"
        )
    })?;
    ensure!(
        matches!(target, TargetTemplate::LocalBare),
        "mj simple requires `{SIMPLE_TARGET_ID}` to be a local bare target"
    );

    let binding = preferences.workspace(&directory);
    Ok(mj_tui::GoMode {
        workspace_id: binding.map(|entry| entry.workspace_id.clone()),
        last_session_id: binding.and_then(|entry| entry.last_session_id.clone()),
        recipe: Some(GoRecipe {
            profile_id,
            target_id: SIMPLE_TARGET_ID.to_owned(),
            bundle_id: None,
            project_directory: Some(directory.clone()),
            create_managed_worktree: Some(true),
            subagents: saved.and_then(|recipe| recipe.subagents),
            additional_mounts: Vec::new(),
            resource_allocation: None,
        }),
        directory,
        save_as_default: false,
        simple: true,
    })
}

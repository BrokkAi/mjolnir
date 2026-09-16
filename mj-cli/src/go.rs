//! Fast-start command preparation; filesystem work runs outside the UI loop.

use anyhow::{Context, Result, ensure};
use clap::Args;
use mj_core::config::{Config, TargetTemplate};
use mj_core::go::{GoPreferences, GoRecipe};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub(crate) struct GoArgs {
    /// Source folder (defaults to the current directory).
    pub folder: Option<PathBuf>,
    /// Change the remembered setup for this folder.
    #[arg(long)]
    pub setup: bool,
    /// Choose a setup and also make it the default for other new projects.
    #[arg(long)]
    pub global_default: bool,
}

pub(crate) fn prepare(args: GoArgs) -> Result<mj_tui::GoMode> {
    use std::io::IsTerminal;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "mj go needs an interactive terminal"
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
    let preferences = GoPreferences::load(&GoPreferences::path())?;
    Ok(mj_tui::GoMode {
        recipe: preferences.recipe(&directory),
        directory,
        save_as_default: args.global_default,
    })
}

pub(crate) fn resolve_recipe(
    directory: &std::path::Path,
    mut recipe: GoRecipe,
) -> Result<(Config, GoRecipe)> {
    let mut config = Config::load()?;
    ensure!(
        config.enabled_profile(&recipe.profile_id).is_some(),
        "Saved account {:?} is unavailable. Use Change setup to choose an account, or F7 Settings to restore it.",
        recipe.profile_id
    );
    let target = config.targets.get(&recipe.target_id).with_context(|| {
        format!(
            "Saved target {:?} is unavailable. Use Change setup or F7 Settings to restore it.",
            recipe.target_id
        )
    })?;
    match target {
        TargetTemplate::LocalBare => recipe.project_directory = Some(directory.to_owned()),
        TargetTemplate::SshBare { .. } => {
            ensure!(
                recipe.project_directory.is_some(),
                "Choose the remote project folder once using Change setup. A local path is not a remote path."
            );
        }
        _ => {
            recipe.project_directory = None;
            if recipe.bundle_id.is_none() {
                let source = directory
                    .to_str()
                    .context("repository path must be valid UTF-8")?;
                let created =
                    mj_controller::controller::create_bundle_from_sources(&[source.to_owned()])?;
                config = created.config;
                recipe.bundle_id = Some(created.bundle_id);
            }
            recipe.create_managed_worktree = Some(false);
        }
    }
    Ok((config, recipe))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn go_accepts_a_folder_and_explicit_default_changes_without_altering_plain_mj() {
        let plain = crate::Cli::try_parse_from(["mj"]).unwrap();
        assert!(plain.command.is_none());
        let parsed =
            crate::Cli::try_parse_from(["mj", "go", "../project", "--global-default"]).unwrap();
        let Some(crate::Command::Go(args)) = parsed.command else {
            panic!("expected go");
        };
        assert_eq!(args.folder.unwrap(), std::path::PathBuf::from("../project"));
        assert!(args.global_default);
    }
}

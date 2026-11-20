//! One serialized setup transaction per instance, including interruption recovery.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use super::{DiscoveredHome, build_config, discover_profiles, reference_secret_api_keys};
use crate::doctor::{CheckStatus, DoctorCheck};
use crate::targets::CommandExecutor;
use mj_client::daemon::FirstRunSetupReport;
use mj_core::config::{Config, atomic_write};

/// Returns `None` when automatic setup belongs to an existing installation.
/// The accepted callback runs under the setup lock, before discovery starts.
pub fn run_setup(
    config_path: &Path,
    instance_data: &Path,
    project_directory: Option<&Path>,
    executor: &impl CommandExecutor,
    cancelled: &dyn Fn() -> bool,
    accepted: impl FnOnce(),
) -> Result<Option<FirstRunSetupReport>> {
    run_setup_with_discovery(config_path, instance_data, cancelled, accepted, || {
        let homes = discover_profiles(executor);
        let cwd = project_directory
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        let repository = cwd.and_then(|cwd| {
            let (source, stripped) =
                mj_core::local_git::discover_project_source(&cwd, executor).ok()?;
            if stripped {
                tracing::warn!("{}", mj_core::remote_git::HTTP_USERINFO_REMOVED);
            }
            Some(source)
        });
        Ok((homes, repository))
    })
}

/// Whether automatic setup has work to do: a new installation, or one whose
/// setup did not finish. A dashboard asks before it shows the first-run
/// welcome, so an existing installation never sees one; `run_setup` then
/// decides again under the setup lock.
pub fn automatic_setup_due(config_path: &Path, instance_data: &Path) -> Result<bool> {
    automatic_setup_is_due(&instance_data.join("setup-state"), config_path)
}

/// The one reading of the setup marker and the configuration.
fn automatic_setup_is_due(marker: &Path, config_path: &Path) -> Result<bool> {
    let state = match std::fs::read(marker) {
        Ok(state) => Some(state),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read setup state"),
    };
    let pending = match state.as_deref() {
        None | Some(b"complete\n") => false,
        Some(b"pending\n") => true,
        Some(_) => bail!("unrecognized setup state in {}", marker.display()),
    };
    let existing = Config::load_from(config_path)?;
    Ok(!(state.as_deref() == Some(b"complete\n") || (!pending && !existing.is_unconfigured())))
}

fn run_setup_with_discovery(
    config_path: &Path,
    instance_data: &Path,
    cancelled: &dyn Fn() -> bool,
    accepted: impl FnOnce(),
    discover: impl FnOnce() -> Result<(Vec<DiscoveredHome>, Option<String>)>,
) -> Result<Option<FirstRunSetupReport>> {
    std::fs::create_dir_all(instance_data).context("create setup state directory")?;
    let _owner = acquire_setup_lock(&instance_data.join("setup.lock"), cancelled)?;
    let marker = instance_data.join("setup-state");
    if !automatic_setup_is_due(&marker, config_path)? {
        return Ok(None);
    }
    ensure!(!cancelled(), "setup cancelled");
    accepted();
    // Persist intent first: if the process dies after saving the config, the
    // next launch must finish setup rather than misclassify it as an old install.
    atomic_write(&marker, b"pending\n")?;
    let (homes, repository) = discover()?;
    ensure!(!cancelled(), "setup cancelled");
    Config::update_to(config_path, |latest| {
        // Choose identifiers against the latest file under its transaction lock.
        // This preserves simultaneous Settings edits and makes reruns additive.
        let mut discovered = build_config(
            &super::without_conventional_duplicates(latest, &homes),
            repository.as_deref(),
        );
        reference_secret_api_keys(&mut discovered, config_path);
        let additions = latest.setup_additions(&discovered);
        latest.profiles.extend(additions.profiles);
        latest.bundles.extend(additions.bundles);
        latest.targets.extend(additions.targets);
        Ok(())
    })?;
    atomic_write(&marker, b"complete\n")?;
    Ok(Some(FirstRunSetupReport {
        agents: homes
            .iter()
            .map(|home| home.kind)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
        repository,
    }))
}

/// Adds a profile for every agent installed since the configuration was last
/// written. Each launch of an existing installation runs this, so a newly
/// installed agent appears without a trip to Settings. A harness that already
/// has a profile does not also gain one from its conventional home, because a
/// managed launch creates that home itself; a home named by the harness's own
/// variable still adds its distinct installation. Settings' Detect profiles
/// imports every installation, including a conventional home. Returns the
/// identifiers it added.
pub fn add_detected_profiles(
    config_path: &Path,
    executor: &impl CommandExecutor,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<String>> {
    add_profiles_for(config_path, &discover_profiles(executor), cancelled)
}

fn add_profiles_for(
    config_path: &Path,
    homes: &[DiscoveredHome],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<String>> {
    ensure!(!cancelled(), "profile detection cancelled");
    // Leave the file untouched on the common launch that finds nothing new.
    let existing = Config::load_from(config_path)?;
    let mut discovered = super::automatic_profiles_config(&existing, homes);
    reference_secret_api_keys(&mut discovered, config_path);
    if existing.setup_additions(&discovered).profiles.is_empty() {
        return Ok(Vec::new());
    }
    let (_, added) = Config::update_to_cancellable(config_path, cancelled, |latest| {
        // Choose identifiers against the latest file under its lock, so a
        // concurrent Settings save is preserved.
        let mut discovered = super::automatic_profiles_config(latest, homes);
        reference_secret_api_keys(&mut discovered, config_path);
        let additions = latest.setup_additions(&discovered).profiles;
        let added = additions.keys().cloned().collect();
        latest.profiles.extend(additions);
        Ok(added)
    })?;
    Ok(added)
}

fn acquire_setup_lock(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<mj_core::file_lock::FileLock> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).context("open setup lock")?;
    mj_core::file_lock::FileLock::acquire_cancellable(file, cancelled).context("lock setup")
}

/// Welcome displays errors and concrete remedies, without successful checks
/// or warnings about optional built-in runtimes and background image downloads.
pub fn welcome_errors(checks: &[DoctorCheck]) -> Vec<String> {
    checks
        .iter()
        // Welcome reports agent discovery and offers setup itself.
        .filter(|check| {
            !matches!(
                check.id.as_str(),
                "config.session-prerequisites" | "harness.discovery" | "harness.profiles"
            )
        })
        .filter(|check| check.status == CheckStatus::Fixable)
        .map(|check| {
            match &check.remediation {
                // The footer has one line: make the remedy visible before long
                // diagnostic context, while retaining that context in the log.
                Some(remediation) => format!("{}: {}\n{}", check.title, remediation, check.detail),
                None => format!("{}: {}", check.title, check.detail),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;

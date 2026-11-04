//! One serialized setup transaction per instance, including interruption recovery.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use super::{
    DiscoveredHome, GithubRepository, add_machine_runtimes, build_config,
    discover_github_repository, discover_profiles, probe_machines, reference_secret_api_keys,
};
use crate::doctor::{
    CheckStatus, DoctorCheck, DoctorOptions, current_apple_platform, probe_executor,
    run_with_config_path,
};
use crate::targets::CommandExecutor;
use mj_core::config::{Config, HarnessKind, atomic_write, data_dir};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupReport {
    pub agents: Vec<HarnessKind>,
    pub repository: Option<String>,
}

impl SetupReport {
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.agents.is_empty() {
            lines.push(format!(
                "Agents: {}.",
                self.agents
                    .iter()
                    .map(|kind| kind.display_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            lines.push(
                "No installed coding agent was found. Mjolnir can install and sign in to one for you."
                    .into(),
            );
        }
        if let Some(repository) = &self.repository {
            lines.push(format!("Project: {repository}."));
        }
        lines
    }
}

/// Returns `None` when automatic setup belongs to an existing installation.
/// The accepted callback runs under the setup lock, before discovery starts.
pub fn run_setup(
    config_path: &Path,
    instance_data: &Path,
    force: bool,
    executor: &impl CommandExecutor,
    cancelled: &dyn Fn() -> bool,
    accepted: impl FnOnce(),
) -> Result<Option<SetupReport>> {
    run_setup_with_discovery(
        config_path,
        instance_data,
        force,
        cancelled,
        accepted,
        || {
            let homes = discover_profiles(executor);
            let cwd = std::env::current_dir().context("read current project directory")?;
            let repository = discover_github_repository(executor, &cwd);
            Ok((homes, repository))
        },
    )
}

fn run_setup_with_discovery(
    config_path: &Path,
    instance_data: &Path,
    force: bool,
    cancelled: &dyn Fn() -> bool,
    accepted: impl FnOnce(),
    discover: impl FnOnce() -> Result<(Vec<DiscoveredHome>, Option<GithubRepository>)>,
) -> Result<Option<SetupReport>> {
    std::fs::create_dir_all(instance_data).context("create setup state directory")?;
    let _owner = acquire_setup_lock(&instance_data.join("setup.lock"), cancelled)?;
    let marker = instance_data.join("setup-state");
    let state = match std::fs::read(&marker) {
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
    if !force
        && (state.as_deref() == Some(b"complete\n") || (!pending && !existing.is_unconfigured()))
    {
        return Ok(None);
    }
    ensure!(!cancelled(), "setup cancelled");
    accepted();
    // Persist intent first: if the process dies after saving the config, the
    // next launch must finish setup rather than misclassify it as an old install.
    atomic_write(&marker, b"pending\n")?;
    let (homes, repository) = discover()?;
    ensure!(!cancelled(), "setup cancelled");
    let mut discovered = build_config(&homes, repository.as_ref());
    reference_secret_api_keys(&mut discovered, config_path);
    Config::update_to(config_path, |latest| {
        // Choose identifiers against the latest file under its transaction lock.
        // This preserves simultaneous Settings edits and makes reruns additive.
        let additions = latest.setup_additions(&discovered);
        latest.profiles.extend(additions.profiles);
        latest.bundles.extend(additions.bundles);
        latest.targets.extend(additions.targets);
        Ok(())
    })?;
    atomic_write(&marker, b"complete\n")?;
    Ok(Some(SetupReport {
        agents: homes
            .iter()
            .map(|home| home.kind)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
        repository: repository.map(|repository| repository.source()),
    }))
}

/// Adds a profile for every agent installed since the configuration was last
/// written, the same additive merge as Settings' Detect profiles. Each launch
/// of an existing installation runs this, so a newly installed agent appears
/// without a trip to Settings. This runs on every launch by the user's
/// decision; a removed profile for an installed agent returns, exactly as it
/// does from Settings' Detect profiles, and disabling the profile is how a
/// user stops using an agent. Returns the identifiers it added.
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
    let mut discovered = super::profiles_config(homes);
    reference_secret_api_keys(&mut discovered, config_path);
    ensure!(!cancelled(), "profile detection cancelled");
    // Leave the file untouched on the common launch that finds nothing new.
    if Config::load_from(config_path)?
        .setup_additions(&discovered)
        .profiles
        .is_empty()
    {
        return Ok(Vec::new());
    }
    let (_, added) = Config::update_to(config_path, |latest| {
        // Choose identifiers against the latest file under its lock, so a
        // concurrent Settings save is preserved.
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
    loop {
        ensure!(!cancelled(), "setup cancelled");
        match file.try_lock() {
            Ok(()) => return Ok(mj_core::file_lock::FileLock::new(file)),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
            Err(std::fs::TryLockError::Error(error)) => return Err(error).context("lock setup"),
        }
    }
}

/// Welcome displays errors and concrete remedies, without successful checks
/// or warnings about optional built-in runtimes and background image downloads.
pub fn actionable_errors(checks: &[DoctorCheck]) -> Vec<String> {
    checks
        .iter()
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

pub fn run_setup_command(config_path: &Path) -> Result<()> {
    let probes = probe_executor();
    let report = run_setup(config_path, &data_dir(), true, &probes, &|| false, || {})?
        .context("explicit setup was skipped")?;
    let mut output = io::stdout().lock();
    writeln!(output, "Welcome to Mjolnir")?;
    for line in report.summary() {
        writeln!(output, "{line}")?;
    }
    // A machine is a session target only through its runtimes, so setup adds
    // every runtime kind each machine newly runs, as the dashboard's check does.
    let machines: Vec<_> = Config::load_from(config_path)?
        .machines
        .into_iter()
        .filter(|(_, machine)| !matches!(machine, mj_core::config::Machine::Local { .. }))
        .collect();
    let found = probe_machines(&machines, &probes);
    for (machine, runtimes) in add_machine_runtimes(config_path, &found, &|| false)? {
        writeln!(output, "Runtimes for {machine}: {}.", runtimes.join(", "))?;
    }
    let checks = run_with_config_path(
        config_path,
        &probes,
        current_apple_platform(&probes),
        DoctorOptions { smoke: false },
    );
    for error in actionable_errors(&checks) {
        writeln!(output, "\n{error}")?;
    }
    writeln!(
        output,
        "\nRun `mj` and press {} to create a session.",
        Config::load_from(config_path)?
            .keybinds()
            .labels(mj_core::config::KeyAction::NewSession)
            .first()
            .map(String::as_str)
            .unwrap_or("the New session command")
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;

//! One serialized setup transaction per instance, including interruption recovery.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use super::{
    DiscoveredHome, GithubRepository, build_config, discover_github_repository, discover_profiles,
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
    let discovered = build_config(&homes, repository.as_ref());
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

fn acquire_setup_lock(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<File> {
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
            Ok(()) => return Ok(file),
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

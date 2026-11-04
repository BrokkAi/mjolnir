//! What is there: the one check that decides which profiles and runtimes the
//! session pickers offer.
//!
//! The dashboard runs this when it starts and whenever a new-session or
//! resume picker opens, off its event loop. It adds what is new (a profile
//! for an agent installed since the configuration was written, a runtime for
//! an engine a machine newly runs) and reports, for everything configured,
//! whether it is there. This computer is checked directly; each SSH machine
//! with one SSH call (`probe_machine`).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, ensure};
use mj_core::config::{AuthScheme, Config, HarnessProfile, Machine};

use super::machines::{MachineProbe, add_machine_runtimes, probe_machines};
use super::{RuntimeProbe, add_detected_profiles, discover_runtimes, harness_cli_installed};
use crate::targets::CommandExecutor;

/// What one availability check found and changed.
#[derive(Debug, Clone)]
pub struct AvailabilityReport {
    /// Every configured profile, as it was checked, and why it is missing,
    /// or `None` when it is there.
    pub profiles: BTreeMap<String, (HarnessProfile, Option<String>)>,
    /// This computer's container engines, usable or not.
    pub local: Vec<RuntimeProbe>,
    /// What each SSH and EC2 machine can run.
    pub machines: Vec<MachineProbe>,
    /// The profiles this check added.
    pub added_profiles: Vec<String>,
    /// The runtimes this check added, by machine.
    pub added_runtimes: BTreeMap<String, Vec<String>>,
}

/// Why `profile` cannot be used here, or `Ok` when it can. A profile is
/// there when a harness can start from it (`HarnessProfile::ensure_ready`,
/// which every launch asks) and, for an agent that keeps its login in its
/// home, that installation still exists: the home, or the agent's own
/// command. Mjolnir runs its own pinned bridges, so a profile whose command
/// was uninstalled but whose home remains is still there.
pub fn profile_presence(
    id: &str,
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> std::result::Result<(), String> {
    profile
        .ensure_ready(id)
        .map_err(|error| format!("{error:#}"))?;
    let logs_in_at_home = matches!(
        profile.auth_scheme(),
        AuthScheme::NativeLogin | AuthScheme::NativeLoginStore
    );
    // A probe that could not run says nothing, so only a command known to
    // be absent makes the profile missing.
    if logs_in_at_home
        && !profile.home.is_dir()
        && harness_cli_installed(profile.kind, executor) == Some(false)
    {
        let name = profile.kind.display_name();
        return Err(format!(
            "{name} is not installed here: {} does not exist and `{}` is not on PATH. Install {name}, or turn the profile off in Settings › Profiles.",
            profile.home.display(),
            profile.kind.cli_binary_name()
        ));
    }
    Ok(())
}

/// Check everything at once and add what is new, under the configuration
/// lock against the latest file. Nothing is removed: what is missing is
/// reported, so it can come back without the user configuring it again.
pub fn check_availability(
    config_path: &Path,
    executor: &(impl CommandExecutor + Sync),
    cancelled: &dyn Fn() -> bool,
) -> Result<AvailabilityReport> {
    let machines: Vec<(String, Machine)> = Config::load_from(config_path)?
        .machines
        .into_iter()
        .filter(|(_, machine)| !matches!(machine, Machine::Local { .. }))
        .collect();
    // Each machine is its own host and this computer is another, so they
    // are all asked at once.
    let (added_profiles, local, machines) = std::thread::scope(|scope| {
        let remote = scope.spawn(|| probe_machines(&machines, executor));
        let local = scope.spawn(|| discover_runtimes(executor));
        let added = add_detected_profiles(config_path, executor, cancelled);
        (added, join(local), join(remote))
    });
    let added_profiles = added_profiles?;
    ensure!(!cancelled(), "availability check cancelled");
    let added_runtimes = add_machine_runtimes(config_path, &machines, cancelled)?;
    let profiles = Config::load_from(config_path)?
        .profiles
        .into_iter()
        .map(|(id, profile)| {
            let missing = profile_presence(&id, &profile, executor).err();
            (id, (profile, missing))
        })
        .collect();
    Ok(AvailabilityReport {
        profiles,
        local,
        machines,
        added_profiles,
        added_runtimes,
    })
}

/// A probe thread's result, re-raising its panic: a scope re-raises only
/// the panics of threads nobody joined.
fn join<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

//! Actionable host and configuration prerequisite checks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;

use crate::controller::{WorkerBinaryAvailability, worker_binary_prerequisite_for_arch};
use crate::setup::{
    DiscoveredHome, discover_harness_homes_with_executor, harness_is_authenticated_with_executor,
};
use crate::targets::{
    BoundedProcessExecutor, CommandExecutor, CommandSpec, CommandTimedOut,
    ContainerTemplate as RuntimeContainerTemplate, PODMAN_DOCUMENTATION_URL, PodmanProbe,
    ProcessExecutor, SshTarget as RuntimeSshTarget, TargetTemplate as RuntimeTargetTemplate,
    failed_podman_probe, podman_probe_observation, run_setup_smoke_test, ssh_command,
    ssh_connectivity_probe, ssh_validation_command, verify_local_docker, verify_local_podman,
    verify_ssh_docker, verify_ssh_podman,
};
use mj_core::config::{
    Config, ContainerTemplate, HarnessHost, HarnessKind, HarnessProfile, TargetTemplate,
    config_path,
};
use mj_core::credentials::login_command;

// Only the image for the Apple container smoke test when the config has no
// apple-container target. This intentionally stays a small stock image rather
// than setup::DEFAULT_IMAGE: the check just proves the runtime can start a
// container, and pulling the multi-gigabyte agent-dev image to do that would be
// a poor trade.
const DEFAULT_CONTAINER_IMAGE: &str = "ubuntu:24.04";
const APPLE_CONTAINER_INSTALL_URL: &str = "https://github.com/apple/container#initial-install";

/// How long a single prerequisite probe may take before doctor reports it as a
/// fixable check instead of waiting for it.
///
/// Every probe outside the opt-in smoke tests is a local or short network call,
/// so this only ever fires for a wedged runtime socket, a blackholed network,
/// or a credential helper waiting on something that will never arrive.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The executor `mj doctor` and `mj setup` run their prerequisite probes
/// through: one deadline per probe, so a wedged runtime cannot hang the run.
pub const fn probe_executor() -> BoundedProcessExecutor {
    BoundedProcessExecutor::new(PROBE_TIMEOUT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ready,
    Warning,
    Fixable,
    Unsupported,
}

impl CheckStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Warning => "warning",
            Self::Fixable => "fixable",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
    pub remediation: Option<String>,
}

impl DoctorCheck {
    fn ready(id: impl Into<String>, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Ready,
            detail: detail.into(),
            remediation: None,
        }
    }

    fn warning(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Warning,
            detail: detail.into(),
            remediation: Some(remediation.into()),
        }
    }

    pub(crate) fn fixable(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Fixable,
            detail: detail.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn unsupported(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status: CheckStatus::Unsupported,
            detail: detail.into(),
            remediation: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorOptions {
    pub smoke: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplePlatform {
    Linux,
    Macos {
        architecture: String,
        major_version: u32,
    },
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionsPlatform {
    Linux,
    Macos,
}

pub fn run_current(options: DoctorOptions) -> Vec<DoctorCheck> {
    if options.smoke {
        // A smoke test may legitimately pull a multi-gigabyte image, which no
        // probe deadline could tell apart from a hung runtime, so an opt-in
        // `--smoke` run keeps waiting for its commands.
        return run_with(
            &ProcessExecutor,
            current_apple_platform(&ProcessExecutor),
            options,
        );
    }
    let executor = probe_executor();
    run_with(&executor, current_apple_platform(&executor), options)
}

pub fn run_with(
    executor: &impl CommandExecutor,
    apple_platform: ApplePlatform,
    options: DoctorOptions,
) -> Vec<DoctorCheck> {
    run_with_config_path(&config_path(), executor, apple_platform, options)
}

/// The same checks as [`run_with`], against an explicit configuration file.
///
/// `mj setup` uses this to report on the configuration it just wrote, so a
/// first run ends with exactly the summary and remediations `mj doctor`
/// would print.
pub fn run_with_config_path(
    config_path: &Path,
    executor: &impl CommandExecutor,
    apple_platform: ApplePlatform,
    options: DoctorOptions,
) -> Vec<DoctorCheck> {
    let (loaded, mut checks) = configuration_checks(config_path);
    let config: ConfigStatus<'_> = loaded.as_ref().map_err(|gap| *gap);
    checks.push(harness_discovery_check(config, executor));
    checks.extend(harness_checks(config, executor));
    checks.extend(subagent_eligibility_checks(config));
    checks.extend(podman_checks(config, executor, options.smoke));
    checks.extend(docker_checks(config, executor, options.smoke));
    checks.extend(ssh_bare_checks(config, executor));
    checks.extend(ssh_podman_checks(config, executor, options.smoke));
    checks.extend(ssh_docker_checks(config, executor, options.smoke));
    checks.extend(build_cache_checks(config, executor));
    checks.extend(aws_checks(config, executor));
    checks.extend(worker_binary_checks(config));
    checks.push(daemon_build_check());
    checks.extend(worker_freshness_checks(config));
    checks.extend(review_residue_checks(config));
    checks.push(apple_container_check(
        &apple_platform,
        executor,
        options.smoke,
        apple_container_image(config),
    ));
    checks
}

fn build_cache_checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    crate::controller::doctor_host_mbx(config, executor)
        .into_iter()
        .map(|host| {
            let id = format!("build-cache.{}", host.host);
            let title = format!("Build cache on {}", host.host);
            let targets = host.targets.join(", ");
            match host.status {
                crate::controller::DoctorHostMbxStatus::Absent => DoctorCheck::ready(
                    id,
                    title,
                    format!(
                        "No native mbx is installed; targets {targets} can use Mjolnir's mbx {}.",
                        crate::controller::MBX_VERSION
                    ),
                ),
                crate::controller::DoctorHostMbxStatus::Compatible(version) => DoctorCheck::ready(
                    id,
                    title,
                    format!(
                        "Host mbx {version} is compatible with Mjolnir's mbx {} for targets {targets}.",
                        crate::controller::MBX_VERSION
                    ),
                ),
                crate::controller::DoctorHostMbxStatus::TooOld(version) => DoctorCheck::warning(
                    id,
                    title,
                    format!(
                        "Host mbx {version} is older than Mjolnir's mbx {}; sessions on targets {targets} run without the shared build cache.",
                        crate::controller::MBX_VERSION
                    ),
                    format!(
                        "Upgrade mbx on {} to {} or newer, then rerun `mj doctor`.",
                        host.host,
                        crate::controller::MBX_VERSION
                    ),
                ),
                crate::controller::DoctorHostMbxStatus::Unknown(error) => DoctorCheck::warning(
                    id,
                    title,
                    format!("Could not check host mbx for targets {targets}: {error}"),
                    format!(
                        "Check access to {} and run `mbx --version` there, then rerun `mj doctor`.",
                        host.host
                    ),
                ),
            }
        })
        .collect()
}

fn harness_discovery_check(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let home = dirs::home_dir();
    let overrides = HarnessKind::ALL.into_iter().filter_map(|kind| {
        std::env::var_os(kind.home_env()).map(|path| (kind, kind.home_from_environment(path)))
    });
    let discovered = discover_harness_homes_with_executor(home.as_deref(), overrides, executor);
    harness_discovery_check_from(
        &discovered,
        config.is_ok_and(|config| !config.profiles.is_empty()),
        &settings_key(config.ok()),
    )
}

fn harness_discovery_check_from(
    discovered: &[DiscoveredHome],
    has_configured_profiles: bool,
    settings_key: &str,
) -> DoctorCheck {
    if discovered.is_empty() {
        return if has_configured_profiles {
            DoctorCheck::ready(
                "harness.discovery",
                "Harness home discovery",
                "No default or environment-overridden harness homes were found; configured profile homes are checked below.",
            )
        } else {
            DoctorCheck::fixable(
                "harness.discovery",
                "Harness home discovery",
                "No Codex, Claude Code, Kimi Code, or Grok Build home was found in the default or environment-overridden locations.",
                format!(
                    "Install and sign in to a supported harness, then open Mjolnir, press {settings_key} for Settings, and choose Agent Profiles."
                ),
            )
        };
    }

    let homes = discovered
        .iter()
        .map(|home| {
            let authentication = if home.authenticated {
                "authenticated"
            } else {
                "not authenticated"
            };
            format!(
                "{} at {} ({authentication})",
                home.kind.display_name(),
                home.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    DoctorCheck::ready(
        "harness.discovery",
        "Harness home discovery",
        format!("Discovered {homes}. Configured profile authentication is checked below."),
    )
}

/// The key that opens Settings, as the help overlay labels it (`ctrl+b s`
/// by default). Without a readable configuration the default bindings apply.
fn settings_key(config: Option<&Config>) -> String {
    let keybinds = config.map_or_else(mj_core::config::Keybinds::default, Config::keybinds);
    keybinds
        .labels(mj_core::config::KeyAction::OpenSettings)
        .into_iter()
        .next()
        .unwrap_or_else(|| "the Settings command in the command palette".to_owned())
}

pub fn all_ready(checks: &[DoctorCheck]) -> bool {
    checks
        .iter()
        .all(|check| check.status != CheckStatus::Fixable)
}

pub fn render_human(checks: &[DoctorCheck], output: &mut impl Write) -> Result<()> {
    for check in checks {
        writeln!(
            output,
            "{} {}: {}",
            check.status.label(),
            check.title,
            check.detail
        )?;
        if let Some(remediation) = &check.remediation {
            writeln!(output, "  remediation: {remediation}")?;
        }
    }
    Ok(())
}

pub fn setup_instructions(platform: InstructionsPlatform) -> String {
    match platform {
        InstructionsPlatform::Linux => format!(
            "# Mjolnir setup instructions for Linux\n\n\
This page is self-contained. Follow this exact loop as the user who will run `mj`:\n\n\
1. Run `mj doctor --json`.\n\
2. Follow every `fixable` remediation from its JSON output.\n\
3. Run `mj doctor --json` again. Repeat until no check is `fixable`.\n\
4. Finish with `mj doctor --json --smoke` to verify every configured container\n\
   image end to end, and resolve anything it reports as `fixable`.\n\n\
For a coding-agent handoff, provide this entire instructions page together with\n\
the latest `mj doctor --json` output.\n\n\
## Local bare runtime\n\n\
A local bare runtime runs the agent directly on this machine. It needs the\n\
native `mj-worker` installed beside `mj`; the release installer and the npm\n\
package include it. The worker installs the pinned harness version itself.\n\
Codex and Claude need Node.js 22 or newer and npm on `PATH`. Kimi and Grok\n\
need curl and Bash. Muse needs curl and tar. Mjolnir does not install these\n\
prerequisites.\n\n\
## Linux container-runtime postconditions\n\n{}\n\n{}",
            crate::targets::PODMAN_DOCUMENTATION,
            crate::targets::DOCKER_DOCUMENTATION
        ),
        InstructionsPlatform::Macos => format!(
            "# Mjolnir setup instructions for macOS\n\n\
This page is self-contained. Follow this exact loop as the user who will run `mj`:\n\n\
1. Run `mj doctor --json`.\n\
2. Follow every `fixable` remediation from its JSON output.\n\
3. Run `mj doctor --json` again. Repeat until no check is `fixable`.\n\n\
For a coding-agent handoff, provide this entire instructions page together with\n\
the latest `mj doctor --json` output.\n\n\
## Local bare runtime\n\n\
A local bare runtime runs the agent directly on this machine. It needs the\n\
native `mj-worker` installed beside `mj`; the release installer and the npm\n\
package include it. The worker installs the pinned harness version itself.\n\
Codex and Claude need Node.js 22 or newer and npm on `PATH`. Kimi and Grok\n\
need curl and Bash. Muse needs curl and tar. Mjolnir does not install these\n\
prerequisites.\n\n\
## Apple container runtime\n\n\
Mjolnir's Apple container target requires Apple silicon and macOS 26 or newer.\n\
On an Intel Mac or an older macOS release, the target is unsupported; use the\n\
local bare runtime, an SSH target, or an AWS target instead.\n\n\
If the `container` command is absent, install only the official signed package:\n\n\
<https://github.com/apple/container#initial-install>\n\n\
Mjolnir never downloads or installs that package. If doctor reports a stopped\n\
daemon, run exactly:\n\n```console\ncontainer system start\n```\n\n\
Finish with the opt-in disposable runtime test in JSON mode:\n\n```console\nmj doctor --json --smoke\n```\n\n\
Apple container is ready only when that smoke test creates a disposable\n\
container, executes `true` in it, and removes it successfully. Use the image\n\
configured by an `apple-container` target; without one, doctor uses\n\
`{DEFAULT_CONTAINER_IMAGE}` for the smoke test.\n\n\
## Shared Mjolnir prerequisites\n\n\
`mj doctor --json` also checks the configuration, each configured harness home\n\
and authentication marker, selected container worker binaries, and any relevant\n\
Podman prerequisites. Resolve every `fixable` status before starting a session.\n"
        ),
    }
}

/// Why `mj doctor` has no configuration for the checks that need one.
///
/// The two cases call for opposite advice, so every dependent check is told
/// which one it is: a file this build cannot read because a newer Mjolnir
/// wrote it is not broken, and telling the user to fix or replace it would
/// destroy that build's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigGap {
    /// A newer Mjolnir wrote the file; the value is the version it carries.
    NewerVersion(u32),
    /// The file is missing, or is not valid Mjolnir TOML.
    Unreadable,
}

/// What a dependent check works from: the loaded configuration, or why there
/// is none.
type ConfigStatus<'a> = std::result::Result<&'a Config, ConfigGap>;

/// What a check reports when the configuration came from a newer Mjolnir.
///
/// Nothing about the check can be evaluated, and nothing the user does to
/// `config.toml` would help, so the check skips and names the one real fix.
fn newer_config_skip(id: &str, title: &str, version: u32) -> DoctorCheck {
    DoctorCheck::unsupported(
        id,
        title,
        format!(
            "Skipped: config.toml was written by a newer Mjolnir (config version {version}; this build supports {}). Update Mjolnir to that build or newer.",
            mj_core::config::CONFIG_VERSION
        ),
    )
}

fn configuration_checks(path: &Path) -> (std::result::Result<Config, ConfigGap>, Vec<DoctorCheck>) {
    if !path.exists() {
        return (
            Err(ConfigGap::Unreadable),
            vec![DoctorCheck::fixable(
                "config",
                "Mjolnir configuration",
                format!("{} does not exist", path.display()),
                format!(
                    "Open Mjolnir and press {} for Settings to add an agent profile.",
                    settings_key(None)
                ),
            )],
        );
    }
    // A config a newer build wrote is not broken TOML: replacing it with
    // `mj setup` would discard that build's settings. Say what is actually
    // wrong before the load below reports it as invalid.
    if let Some(found) = mj_core::config::newer_version_on_disk(path) {
        return (
            Err(ConfigGap::NewerVersion(found)),
            vec![DoctorCheck::fixable(
                "config",
                "Mjolnir configuration",
                format!(
                    "{} was written by a newer Mjolnir (config version {found}; this build supports {})",
                    path.display(),
                    mj_core::config::CONFIG_VERSION
                ),
                "Update Mjolnir to that build or newer. Do not lower the version value by hand or replace the file.",
            )],
        );
    }
    match Config::load_from(path) {
        Ok(config) => {
            let mut checks = vec![DoctorCheck::ready(
                "config",
                "Mjolnir configuration",
                format!("{} is valid", path.display()),
            )];
            // A bundle only names a set of repositories to start from; a
            // session can start from any project directory without one, so
            // only the missing profile keeps sessions from starting.
            if config.enabled_profiles().next().is_none() {
                checks.push(DoctorCheck::fixable(
                    "config.session-prerequisites",
                    "Session configuration",
                    "No agent profile is enabled, so no session can start. Local targets are supplied automatically.",
                    format!(
                        "Open Mjolnir and press {} for Settings to add or enable an agent profile.",
                        settings_key(Some(&config))
                    ),
                ));
            } else if config.bundles.is_empty() {
                checks.push(DoctorCheck::ready(
                    "config.session-prerequisites",
                    "Session configuration",
                    "An agent profile is enabled. No project bundle is configured; sessions start from a project directory, and a bundle is only needed to start from a saved set of repositories.",
                ));
            } else {
                checks.push(DoctorCheck::ready(
                    "config.session-prerequisites",
                    "Session configuration",
                    "At least one profile, bundle, and target are configured.",
                ));
            }
            (Ok(config), checks)
        }
        Err(error) => (
            Err(ConfigGap::Unreadable),
            vec![DoctorCheck::fixable(
                "config",
                "Mjolnir configuration",
                format!("{} is invalid: {error:#}", path.display()),
                "Fix the reported TOML error in config.toml, or run `mj setup` to replace it.",
            )],
        ),
    }
}

fn harness_checks(config: ConfigStatus<'_>, executor: &impl CommandExecutor) -> Vec<DoctorCheck> {
    let config = match config {
        Ok(config) => config,
        Err(ConfigGap::NewerVersion(version)) => {
            return vec![newer_config_skip(
                "harness.profiles",
                "Harness profiles",
                version,
            )];
        }
        Err(ConfigGap::Unreadable) => {
            return vec![DoctorCheck::fixable(
                "harness.profiles",
                "Harness profiles",
                "Harness homes cannot be checked until config.toml is valid.",
                "Fix config.toml, then rerun `mj doctor --json`.",
            )];
        }
    };
    if config.profiles.is_empty() {
        return vec![DoctorCheck::fixable(
            "harness.profiles",
            "Harness profiles",
            "No harness profiles are configured.",
            format!(
                "Open Mjolnir, press {} for Settings, and choose Agent Profiles to detect accounts or add a profile.",
                settings_key(Some(config))
            ),
        )];
    }
    config
        .profiles
        .iter()
        .map(|(id, profile)| {
            let title = format!("Harness profile {id}");
            if !profile.enabled {
                return DoctorCheck::ready(
                    format!("harness.{id}"),
                    title,
                    "Profile is disabled; home and authentication checks were skipped.",
                );
            }
            if let Some(default_home) = unscopable_home_is_ignored(config, profile) {
                return DoctorCheck::fixable(
                    format!("harness.{id}"),
                    title,
                    format!(
                        "{} is ignored by a session on this machine: {} on macOS reads {} \
                         whatever {} says",
                        profile.home.display(),
                        profile.kind.display_name(),
                        default_home.display(),
                        profile.kind.home_env(),
                    ),
                    format!(
                        "Set this profile's home to {}, or use it only on container and SSH \
                         targets, where the home is still scoped.",
                        default_home.display()
                    ),
                );
            }
            if !profile.home.is_dir() {
                return DoctorCheck::fixable(
                    format!("harness.{id}"),
                    title,
                    format!("{} does not exist", profile.home.display()),
                    format!(
                        "{} If this profile should use an existing installation, select its home in Setup.",
                        harness_login_remediation(id, profile)
                    ),
                );
            }
            if !harness_is_authenticated_with_executor(profile, executor) {
                return DoctorCheck::fixable(
                    format!("harness.{id}"),
                    title,
                    format!(
                        "No usable authentication was detected for {}",
                        profile.home.display()
                    ),
                    harness_login_remediation(id, profile),
                );
            }
            DoctorCheck::ready(
                format!("harness.{id}"),
                title,
                format!(
                    "{} is present and authentication is available",
                    profile.home.display()
                ),
            )
        })
        .collect()
}

/// The harness's own default home, for a profile whose configured home this
/// machine cannot scope, and `None` when the home is honored as configured.
///
/// Claude Code on macOS is the only such case today: Mjolnir sets no
/// `CLAUDE_CONFIG_DIR` there, so a profile pointing anywhere but Claude's own
/// home would be silently unused. Saying so is better than letting the session
/// run against a home nobody configured.
fn unscopable_home_is_ignored(config: &Config, profile: &HarnessProfile) -> Option<PathBuf> {
    if profile
        .kind
        .scopes_home_with_environment(HarnessHost::current())
    {
        return None;
    }
    // The variable still scopes a home on a container or SSH target, so a
    // profile that can only run there is configured correctly and must not be
    // told to collapse the separation its sessions rely on.
    if !config
        .targets
        .values()
        .any(|target| matches!(target, TargetTemplate::LocalBare))
    {
        return None;
    }
    let default_home = dirs::home_dir()?.join(profile.kind.default_home_leaf());
    (profile.home != default_home).then_some(default_home)
}

/// Warn about a profile that is both listed for sub-agent use and disabled.
///
/// The daemon keeps running and simply does not offer such a profile to a
/// parent, because the delegation candidates and the spawn gate both require an
/// enabled profile. This surfaces the contradiction so the eligible list and
/// the profile's `enabled` flag can be reconciled, rather than leaving a profile
/// the user meant to use silently unavailable.
fn subagent_eligibility_checks(config: ConfigStatus<'_>) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    config
        .subagents
        .eligible_profiles
        .iter()
        .filter(|(_, eligible)| **eligible)
        .filter_map(|(id, _)| {
            let profile = config.profiles.get(id)?;
            (!profile.enabled).then(|| {
                DoctorCheck::warning(
                    format!("subagents.{id}"),
                    format!("Sub-agent profile {id}"),
                    format!(
                        "Profile {id:?} is listed in [subagents.eligible_profiles] but is disabled, so it is not offered for sub-agent use."
                    ),
                    format!(
                        "Re-enable profile {id:?}, or remove it from [subagents.eligible_profiles]."
                    ),
                )
            })
        })
        .collect()
}

/// Point an unauthenticated profile at `mj login`, which already knows how to
/// sign each harness in.
///
/// The underlying command is named only for the reader's benefit; it comes from
/// [`login_command`], the one place that tracks what each harness CLI actually
/// accepts, so this text cannot drift away from what `mj login` runs.
fn harness_login_remediation(id: &str, profile: &HarnessProfile) -> String {
    let (program, arguments) = match login_command(profile) {
        Ok(command) => command,
        // An API-key profile has no login to recommend; say what is missing
        // instead. The authentication gate normally passes such a profile, so
        // this text appears only when its configuration file is absent.
        Err(error) => return format!("{error} Check {}.", profile.home.display()),
    };
    format!(
        "Run `mj login --profile {id}`; it runs `{program} {}` against {}.",
        arguments.join(" "),
        profile.home.display()
    )
}

/// Host Podman prerequisites, then one image check per `local-podman` target.
///
/// The image checks run only after the host preflight passes, because a broken
/// Podman installation already reports its own actionable check.
fn podman_checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let effective = config.map(|config| config.clone().with_local_targets());
    let effective = effective.as_ref().map_err(|gap| *gap);
    let explicit = config.is_ok_and(|config| !local_podman_targets(config).is_empty());
    let preflight = builtin_target_availability(
        podman_check(effective, executor),
        explicit,
        "Podman",
        "podman",
    );
    let preflight_passed = preflight.status == CheckStatus::Ready;
    let mut checks = vec![preflight];
    if preflight_passed {
        checks.extend(
            podman_image_checks(effective, executor, smoke)
                .into_iter()
                .map(|(id, check)| builtin_image_check(config, &id, check)),
        );
    }
    checks
}

/// An image check for a standard local target the user never configured.
/// The dashboard downloads that image itself when it starts, so a missing
/// image is a warning rather than a fault.
fn builtin_image_check(
    config: ConfigStatus<'_>,
    target_id: &str,
    check: DoctorCheck,
) -> DoctorCheck {
    let explicit = config.is_ok_and(|config| config.targets.contains_key(target_id));
    if explicit || check.status != CheckStatus::Fixable {
        return check;
    }
    DoctorCheck::warning(
        check.id,
        check.title,
        format!(
            "{} (built-in `{target_id}` target; the dashboard downloads its image when it starts)",
            check.detail
        ),
        check.remediation.unwrap_or_default(),
    )
}

/// Doctor checks the same target set the dashboard lists: the configured
/// targets plus the standard local ones [`Config::with_local_targets`]
/// supplies whether or not their engine is installed. A standard target whose
/// engine is missing or not running is reported as unavailable, as the
/// dashboard's Targets pane marks it, rather than as a fault to fix: nobody
/// asked for it. A target the user configured keeps the fixable result.
fn builtin_target_availability(
    check: DoctorCheck,
    explicit: bool,
    engine: &str,
    target_id: &str,
) -> DoctorCheck {
    if explicit || check.status != CheckStatus::Fixable {
        return check;
    }
    DoctorCheck::unsupported(
        check.id,
        check.title,
        format!(
            "{engine} is not available, so the built-in `{target_id}` target is marked unavailable: {}",
            check.detail
        ),
    )
}

fn podman_check(config: ConfigStatus<'_>, executor: &impl CommandExecutor) -> DoctorCheck {
    let config = match config {
        Ok(config) => config,
        Err(ConfigGap::NewerVersion(version)) => {
            return newer_config_skip("runtime.podman", "Rootless Podman", version);
        }
        Err(ConfigGap::Unreadable) => {
            return DoctorCheck::unsupported(
                "runtime.podman",
                "Rootless Podman",
                "Podman prerequisites cannot be evaluated until config.toml is valid.",
            );
        }
    };
    if local_podman_targets(config).is_empty() {
        return DoctorCheck::unsupported(
            "runtime.podman",
            "Rootless Podman",
            "No local-podman target is configured.",
        );
    }
    local_podman_runtime_check(executor)
}

/// Probe the local rootless Podman prerequisites and phrase the result as a
/// doctor check.
///
/// This is the single source of truth for Podman availability wording and
/// remediation. `mj setup` calls it directly so its runtime list reports the
/// same detail and fix that `mj doctor` would.
pub fn local_podman_runtime_check(executor: &impl CommandExecutor) -> DoctorCheck {
    match verify_local_podman(executor) {
        Ok(preflight) => DoctorCheck::ready(
            "runtime.podman",
            "Rootless Podman",
            format!("Podman {} has a valid rootless UID map.", preflight.version),
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.podman",
            "Rootless Podman",
            podman_failure_detail(&error),
            podman_remediation(&error),
        ),
    }
}

fn local_podman_targets(config: &Config) -> Vec<(&String, &ContainerTemplate)> {
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::LocalPodman { container } => Some((id, container)),
            _ => None,
        })
        .collect()
}

fn podman_image_checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<(String, DoctorCheck)> {
    let Ok(config) = config else {
        return Vec::new();
    };
    local_podman_targets(config)
        .into_iter()
        .map(|(id, container)| {
            (
                id.clone(),
                podman_image_check(id, &container.image, executor, smoke),
            )
        })
        .collect()
}

fn podman_image_check(
    id: &str,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.podman.image.{id}");
    let title = format!("Podman image for target {id}");
    if smoke {
        let target = RuntimeTargetTemplate::LocalPodman(RuntimeContainerTemplate {
            build_cache: None,
            image: image.to_owned(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        });
        return match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
            Ok(()) => DoctorCheck::ready(
                check_id,
                title,
                format!("Disposable run/exec/remove smoke test passed for image {image}."),
            ),
            Err(error) => DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test failed for image {image}: {error:#}"
                ),
                "Fix the configured image or Podman runtime, then run `mj doctor --json --smoke` again.",
            ),
        };
    }

    let command = CommandSpec::new("podman", ["image", "exists", image])
        .purpose("check Podman image presence");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            check_id,
            title,
            format!("Image {image} is present in local Podman storage."),
        ),
        Ok(_) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Image {image} is not present in local Podman storage."),
            missing_image_remediation(image),
        ),
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Could not check whether image {image} is present in local Podman storage: {error}"
            ),
            missing_image_remediation(image),
        ),
    }
}

fn missing_image_remediation(image: &str) -> String {
    format!(
        "Pull it with `podman pull {image}`, build it from containers/Containerfile.agent-dev, or run `mj doctor --json --smoke` to verify the full pull-and-run path."
    )
}

/// Host Docker prerequisites, then one image check per `local-docker` target.
fn docker_checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let config = match config {
        Ok(config) => config,
        Err(ConfigGap::NewerVersion(version)) => {
            return vec![newer_config_skip("runtime.docker", "Docker", version)];
        }
        Err(ConfigGap::Unreadable) => {
            return vec![DoctorCheck::unsupported(
                "runtime.docker",
                "Docker",
                "Docker prerequisites cannot be evaluated until config.toml is valid.",
            )];
        }
    };
    let explicit = !local_docker_targets(config).is_empty();
    let effective = config.clone().with_local_targets();
    let targets = local_docker_targets(&effective);
    if targets.is_empty() {
        return vec![DoctorCheck::unsupported(
            "runtime.docker",
            "Docker",
            "No local-docker target is configured.",
        )];
    }
    let preflight = builtin_target_availability(
        local_docker_runtime_check(executor),
        explicit,
        "Docker",
        "docker",
    );
    if preflight.status != CheckStatus::Ready {
        return vec![preflight];
    }
    let mut checks = vec![preflight];
    checks.extend(targets.into_iter().map(|(id, container)| {
        builtin_image_check(
            Ok(config),
            id,
            docker_image_check(id, &container.image, executor, smoke),
        )
    }));
    checks
}

pub fn local_docker_runtime_check(executor: &impl CommandExecutor) -> DoctorCheck {
    match verify_local_docker(executor) {
        Ok(preflight) => DoctorCheck::ready(
            "runtime.docker",
            "Docker",
            format!(
                "Docker {} is connected to a Linux daemon.",
                preflight.version
            ),
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.docker",
            "Docker",
            format!("{error:#}"),
            "Install and start Docker, then make sure `docker info` succeeds as the user running mj.",
        ),
    }
}

fn local_docker_targets(config: &Config) -> Vec<(&String, &ContainerTemplate)> {
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::LocalDocker { container } => Some((id, container)),
            _ => None,
        })
        .collect()
}

fn docker_image_check(
    id: &str,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.docker.image.{id}");
    let title = format!("Docker image for target {id}");
    if smoke {
        let target = RuntimeTargetTemplate::LocalDocker(RuntimeContainerTemplate {
            build_cache: None,
            image: image.to_owned(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        });
        return match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
            Ok(()) => DoctorCheck::ready(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove and OverlayFS attachment smoke test passed for image {image}."
                ),
            ),
            Err(error) => DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test failed for image {image}: {error:#}"
                ),
                "Fix the configured image or Docker runtime, then run `mj doctor --json --smoke` again.",
            ),
        };
    }
    let command = CommandSpec::new("docker", ["image", "inspect", image])
        .purpose("check Docker image presence");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            check_id,
            title,
            format!("Image {image} is present in Docker storage."),
        ),
        Ok(_) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Image {image} is not present in Docker storage."),
            format!("Pull it with `docker pull {image}`, or run `mj doctor --json --smoke`."),
        ),
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Could not inspect Docker image {image}: {error}"),
            format!("Make sure `docker info` succeeds, then run `docker pull {image}`."),
        ),
    }
}

/// The outcome of the shared SSH connectivity probe.
///
/// Both SSH-backed checks run this first: an unreachable host makes every
/// later probe fail with a misleading message.
enum SshConnectivity {
    Reachable,
    Failed { detail: String, remediation: String },
}

/// Probe `ssh <destination> true` and map any failure to a copy-paste fix.
///
/// Hel never generates keys, runs `ssh-copy-id`, or accepts a host key on the
/// user's behalf; it only says exactly which command would fix the failure.
fn ssh_connectivity(ssh: &RuntimeSshTarget, executor: &impl CommandExecutor) -> SshConnectivity {
    let destination = &ssh.destination;
    let command = ssh_connectivity_probe(ssh);
    match executor.execute(&command) {
        Err(error) => SshConnectivity::Failed {
            detail: format!("Could not run `ssh {destination} true`: {error:#}"),
            remediation: ssh_launch_failure_remediation(&error, ssh),
        },
        Ok(output) if output.status != 0 => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            SshConnectivity::Failed {
                detail: format!("`ssh {destination} true` failed: {stderr}"),
                remediation: ssh_failure_remediation(&stderr, ssh),
            }
        }
        Ok(_) => SshConnectivity::Reachable,
    }
}

const SSH_MISSING_REMEDIATION: &str = "Install an OpenSSH client and put `ssh` on PATH: `sudo apt update && sudo apt install -y openssh-client` (Debian/Ubuntu) or `sudo dnf install -y openssh-clients` (Fedora).";

/// What OpenSSH reported, as far as doctor needs to tell the cases apart.
///
/// OpenSSH is an external tool, so its wording is the only signal available.
/// This is the one place in doctor that reads it; everything downstream works
/// from the classification rather than the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshFailure {
    UntrustedHostKey,
    Unauthenticated,
    ClientMissing,
    /// The host answered nothing at all.
    Unreachable,
    Unrecognized,
}

fn classify_ssh_stderr(stderr: &str) -> SshFailure {
    const UNTRUSTED_HOST_KEY: [&str; 3] = [
        "Host key verification failed",
        "No ECDSA host key is known",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
    ];
    const UNAUTHENTICATED: [&str; 4] = [
        "Permission denied",
        "Too many authentication failures",
        "no matching host key",
        "Authentication failed",
    ];
    const CLIENT_MISSING: [&str; 2] = ["ssh: command not found", "No such file or directory"];
    const UNREACHABLE: [&str; 3] = [
        "Connection timed out",
        "No route to host",
        "Network is unreachable",
    ];

    let reported = |signatures: &[&str]| signatures.iter().any(|text| stderr.contains(text));
    if reported(&UNTRUSTED_HOST_KEY) {
        SshFailure::UntrustedHostKey
    } else if reported(&UNAUTHENTICATED) {
        SshFailure::Unauthenticated
    } else if reported(&CLIENT_MISSING) {
        SshFailure::ClientMissing
    } else if reported(&UNREACHABLE) {
        SshFailure::Unreachable
    } else {
        SshFailure::Unrecognized
    }
}

/// Map a failure to run `ssh` at all (as opposed to `ssh` exiting nonzero)
/// to the command that fixes it.
fn ssh_launch_failure_remediation(error: &anyhow::Error, ssh: &RuntimeSshTarget) -> String {
    if error.downcast_ref::<CommandTimedOut>().is_some() {
        return ssh_unreachable_remediation(ssh);
    }
    let missing_binary = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    });
    if missing_binary {
        return SSH_MISSING_REMEDIATION.to_owned();
    }
    format!(
        "Run `ssh {} true` by hand and resolve the error it reports: {error:#}",
        ssh.destination
    )
}

/// The host answered nothing: it is asleep, behind a down VPN, or the cloud
/// session that exposes it has expired.
fn ssh_unreachable_remediation(ssh: &RuntimeSshTarget) -> String {
    let host = ssh_host_only(&ssh.destination);
    format!(
        "Check that {host} is up and reachable from this machine: wake it, bring up the VPN, or refresh the cloud session that exposes it, then run `ssh {} true` by hand.",
        ssh.destination
    )
}

/// Map `ssh -o BatchMode=yes` stderr to the command that fixes it.
fn ssh_failure_remediation(stderr: &str, ssh: &RuntimeSshTarget) -> String {
    let destination = &ssh.destination;
    match classify_ssh_stderr(stderr) {
        SshFailure::UntrustedHostKey => {
            let host = ssh_host_only(destination);
            format!(
                "Add the host key with `ssh-keyscan -H {host} >> ~/.ssh/known_hosts`. Verify the fingerprint out of band before trusting it; if the key changed, remove the stale entry with `ssh-keygen -R {host}` first."
            )
        }
        SshFailure::Unauthenticated => match ssh_identity_file(ssh) {
            Some(identity) => format!(
                "Install your public key on the host with `ssh-copy-id -i {identity}.pub {destination}`."
            ),
            None => {
                format!("Install your public key on the host with `ssh-copy-id {destination}`.")
            }
        },
        SshFailure::ClientMissing => SSH_MISSING_REMEDIATION.to_owned(),
        SshFailure::Unreachable => ssh_unreachable_remediation(ssh),
        SshFailure::Unrecognized => {
            format!(
                "Run `ssh {destination} true` by hand and resolve the error it reports: {stderr}"
            )
        }
    }
}

/// The host part of an OpenSSH destination, without any `user@` prefix.
fn ssh_host_only(destination: &str) -> &str {
    destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host)
}

/// The identity file provisioning passes, recovered from the built ssh args.
fn ssh_identity_file(ssh: &RuntimeSshTarget) -> Option<&str> {
    let position = ssh.ssh_args.iter().position(|arg| arg == "-i")?;
    ssh.ssh_args.get(position + 1).map(String::as_str)
}

/// One check per `ssh-bare` target: can Hel reach the host noninteractively?
fn ssh_bare_checks(config: ConfigStatus<'_>, executor: &impl CommandExecutor) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::SshBare { ssh, .. } => {
                Some(ssh_bare_check(id, &RuntimeSshTarget::from(ssh), executor))
            }
            _ => None,
        })
        .collect()
}

fn ssh_bare_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let check_id = format!("runtime.ssh-bare.{id}");
    let title = format!("SSH access for target {id}");
    match ssh_connectivity(ssh, executor) {
        SshConnectivity::Reachable => DoctorCheck::ready(
            check_id,
            title,
            format!(
                "`ssh {} true` succeeds noninteractively from this host.",
                ssh.destination
            ),
        ),
        SshConnectivity::Failed {
            detail,
            remediation,
        } => DoctorCheck::fixable(check_id, title, detail, remediation),
    }
}

/// Two checks per `ssh-podman` target: the same Podman probes run over SSH,
/// then the host limits that only bite under provisioning load.
fn ssh_podman_checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .flat_map(|(id, target)| match target {
            TargetTemplate::SshPodman { ssh, container, .. } => {
                let ssh = RuntimeSshTarget::from(ssh);
                let (check, reachable) =
                    ssh_podman_check(id, &ssh, &container.image, executor, smoke);
                let mut checks = vec![check];
                // An unreachable host has one problem, not two.
                if reachable {
                    checks.push(ssh_podman_limits_check(id, &ssh, executor));
                }
                checks
            }
            _ => Vec::new(),
        })
        .collect()
}

/// The Podman check for one target, paired with whether the host answered SSH
/// at all: the caller skips its follow-up probes when it did not.
fn ssh_podman_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> (DoctorCheck, bool) {
    let check_id = format!("runtime.ssh-podman.{id}");
    let title = format!("Remote Podman for target {id}");
    // Connectivity first: a remote Podman probe on an unreachable host reports
    // a Podman problem the user does not have.
    if let SshConnectivity::Failed {
        detail,
        remediation,
    } = ssh_connectivity(ssh, executor)
    {
        return (
            DoctorCheck::fixable(check_id, title, detail, remediation),
            false,
        );
    }
    (
        ssh_podman_runtime_check(check_id, title, ssh, image, executor, smoke),
        true,
    )
}

/// The Podman half of the target's checks, on a host already known reachable.
fn ssh_podman_runtime_check(
    check_id: String,
    title: String,
    ssh: &RuntimeSshTarget,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let destination = &ssh.destination;
    let preflight = match verify_ssh_podman(ssh, executor) {
        Ok(preflight) => preflight,
        Err(error) => {
            let detail = podman_failure_detail(&error);
            let remediation = match podman_remediation_match(&error) {
                Some(remediation) => {
                    format!("On {destination}: {remediation} See {PODMAN_DOCUMENTATION_URL}.")
                }
                None => format!(
                    "Verify `ssh {destination}` succeeds noninteractively from this host, then install rootless Podman 4.3 or newer there. See {PODMAN_DOCUMENTATION_URL}."
                ),
            };
            return DoctorCheck::fixable(check_id, title, detail, remediation);
        }
    };
    let linger_warning = preflight.warnings.first();
    if !smoke && let Some(warning) = linger_warning {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "Remote rootless Podman {} is available via {destination}, but {}",
                preflight.version, warning.detail
            ),
            &warning.remediation,
        );
    }
    if !smoke {
        return DoctorCheck::ready(
            check_id,
            title,
            format!(
                "Remote rootless Podman {} is available via {destination}. Run `mj doctor --json --smoke` to verify the image end to end.",
                preflight.version
            ),
        );
    }

    let target = RuntimeTargetTemplate::SshPodman {
        ssh: ssh.clone(),
        container: RuntimeContainerTemplate {
            build_cache: None,
            image: image.to_owned(),
            pull_policy: Default::default(),
            extra_run_args: vec![],
            workspace_storage: Default::default(),
        },
    };
    match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
        Ok(()) => match linger_warning {
            Some(warning) => DoctorCheck::warning(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test passed for image {image} on {destination}, but {}",
                    warning.detail
                ),
                &warning.remediation,
            ),
            None => DoctorCheck::ready(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test passed for image {image} on {destination}."
                ),
            ),
        },
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Disposable run/exec/remove smoke test failed for image {image} on {destination}: {error:#}"
            ),
            format!(
                "Fix the configured image or Podman runtime on {destination}, then run `mj doctor --json --smoke` again."
            ),
        ),
    }
}

/// Host limits that cause provisioning failures under load, read on their own SSH
/// round trip so the provisioning preflight never pays for them.
///
/// Every crun container takes a session keyring, so `podman run` fails with
/// `crun: create keyring` once the login user's keyring quota is exhausted, and
/// sshd refuses new connections past `MaxStartups`. `sshd -T` needs root, so the
/// directive is read from the config files instead; drop-ins may be unreadable,
/// which the script reports rather than guessing.
const SSH_PODMAN_HOST_LIMITS_SCRIPT: &str = r#"
if [ -r /proc/sys/kernel/keys/maxkeys ]; then
    printf 'keys.max=%s\n' "$(cat /proc/sys/kernel/keys/maxkeys)"
fi
if [ -r /proc/key-users ]; then
    awk -v uid="$(id -u)" '
        { user = $1; sub(/:$/, "", user) }
        user == uid {
            split($4, quota, "/")
            printf "keys.used=%s\nkeys.quota=%s\n", quota[1], quota[2]
        }
    ' /proc/key-users
fi
unreadable=0
maxstartups=
# A drop-in directory that cannot be listed hides any override it holds.
if [ -d /etc/ssh/sshd_config.d ] && ! [ -r /etc/ssh/sshd_config.d ]; then
    unreadable=1
fi
for file in /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf; do
    [ -e "$file" ] || continue
    if [ -r "$file" ]; then
        match=$(grep -i '^[[:space:]]*maxstartups[[:space:]]' "$file" 2>/dev/null | tail -n 1)
        [ -n "$match" ] && maxstartups=$(printf '%s\n' "$match" | awk '{ print $2 }')
    else
        unreadable=1
    fi
done
[ -n "$maxstartups" ] && printf 'maxstartups=%s\n' "$maxstartups"
[ "$unreadable" = 1 ] && printf 'maxstartups.unreadable=1\n'
exit 0
"#;

/// Keyring use at or above this share of the quota is reported as a warning:
/// the remaining headroom is a few concurrent containers, not a comfortable
/// margin.
const KEYRING_PRESSURE_PERCENT: u64 = 80;

/// What `SSH_PODMAN_HOST_LIMITS_SCRIPT` managed to read. Every field is
/// optional: an unreadable file is reported, never guessed at.
#[derive(Debug, Default, PartialEq, Eq)]
struct HostLimits {
    keys_used: Option<u64>,
    keys_quota: Option<u64>,
    keys_max: Option<u64>,
    max_startups: Option<String>,
    max_startups_unreadable: bool,
}

fn parse_host_limits(stdout: &[u8]) -> HostLimits {
    let text = String::from_utf8_lossy(stdout);
    let mut limits = HostLimits::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match name.trim() {
            "keys.used" => limits.keys_used = value.parse().ok(),
            "keys.quota" => limits.keys_quota = value.parse().ok(),
            "keys.max" => limits.keys_max = value.parse().ok(),
            "maxstartups" if !value.is_empty() => limits.max_startups = Some(value.to_owned()),
            "maxstartups.unreadable" => limits.max_startups_unreadable = value == "1",
            _ => {}
        }
    }
    limits
}

impl HostLimits {
    /// True when the script produced nothing a reader could act on.
    fn is_empty(&self) -> bool {
        self.keys_used.is_none()
            && self.keys_quota.is_none()
            && self.keys_max.is_none()
            && self.max_startups.is_none()
            && !self.max_startups_unreadable
    }

    fn keyring_is_under_pressure(&self) -> bool {
        match (self.keys_used, self.keys_quota) {
            (Some(used), Some(quota)) if quota > 0 => {
                used.saturating_mul(100) >= quota.saturating_mul(KEYRING_PRESSURE_PERCENT)
            }
            _ => false,
        }
    }

    fn keyring_sentence(&self, destination: &str) -> String {
        match (self.keys_used, self.keys_quota) {
            (Some(used), Some(quota)) => {
                let system = match self.keys_max {
                    Some(max) => format!(", and `kernel.keys.maxkeys` is {max}"),
                    None => String::new(),
                };
                format!(
                    "The login user on {destination} holds {used} of its {quota} kernel keyring quota{system}."
                )
            }
            _ => format!(
                "The kernel keyring quota for the login user on {destination} could not be read."
            ),
        }
    }

    fn max_startups_sentence(&self) -> String {
        match (&self.max_startups, self.max_startups_unreadable) {
            (Some(value), _) => format!("sshd MaxStartups is {value}."),
            (None, true) => "sshd MaxStartups is not set in a readable sshd_config file, so sshd's default applies unless an unreadable drop-in overrides it.".to_owned(),
            (None, false) => {
                "sshd MaxStartups is not set in sshd_config, so sshd's default applies.".to_owned()
            }
        }
    }
}

/// Report the two host limits that made provisioning fail under load. The
/// target still works when they cannot be read, so an unreadable host is a
/// warning with a manual command, never a `fixable` runtime failure.
fn ssh_podman_limits_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let check_id = format!("runtime.ssh-podman.{id}.limits");
    let title = format!("Host limits for target {id}");
    let destination = &ssh.destination;
    let manual = || {
        format!(
            "Read them by hand on {destination}: `cat /proc/key-users /proc/sys/kernel/keys/maxkeys` and `grep -ri maxstartups /etc/ssh/sshd_config /etc/ssh/sshd_config.d`."
        )
    };
    let command = ssh_validation_command(
        ssh,
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            SSH_PODMAN_HOST_LIMITS_SCRIPT.to_owned(),
        ],
        "read ssh-podman host limits",
    );
    let limits = match executor.execute(&command) {
        Ok(output) if output.status == 0 => parse_host_limits(&output.stdout),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return DoctorCheck::warning(
                check_id,
                title,
                format!(
                    "Could not read the kernel keyring quota or sshd MaxStartups from {destination}: {stderr}"
                ),
                manual(),
            );
        }
        Err(error) => {
            return DoctorCheck::warning(
                check_id,
                title,
                format!(
                    "Could not read the kernel keyring quota or sshd MaxStartups from {destination}: {error}"
                ),
                manual(),
            );
        }
    };
    if limits.is_empty() {
        return DoctorCheck::warning(
            check_id,
            title,
            format!("{destination} reported no readable kernel keyring or sshd limits."),
            manual(),
        );
    }
    let detail = format!(
        "{} {}",
        limits.keyring_sentence(destination),
        limits.max_startups_sentence()
    );
    if limits.keyring_is_under_pressure() {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "{detail} Every container takes a session keyring, so `podman run` fails with `crun: create keyring` once the quota is gone."
            ),
            format!(
                "Raise `kernel.keys.maxkeys` and `kernel.keys.maxbytes` with sysctl on {destination}, and close finished sessions promptly."
            ),
        );
    }
    DoctorCheck::ready(check_id, title, detail)
}

/// One check per `ssh-docker` target: Docker daemon, image, and optional
/// remote OverlayFS smoke test, all executed on the SSH host.
fn ssh_docker_checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::SshDocker { ssh, container } => Some(ssh_docker_check(
                id,
                &RuntimeSshTarget::from(ssh),
                &container.image,
                executor,
                smoke,
            )),
            _ => None,
        })
        .collect()
}

fn ssh_docker_check(
    id: &str,
    ssh: &RuntimeSshTarget,
    image: &str,
    executor: &impl CommandExecutor,
    smoke: bool,
) -> DoctorCheck {
    let check_id = format!("runtime.ssh-docker.{id}");
    let title = format!("Remote Docker for target {id}");
    let destination = &ssh.destination;
    if let SshConnectivity::Failed {
        detail,
        remediation,
    } = ssh_connectivity(ssh, executor)
    {
        return DoctorCheck::fixable(check_id, title, detail, remediation);
    }

    let preflight = match verify_ssh_docker(ssh, executor) {
        Ok(preflight) => preflight,
        Err(error) => {
            let detail = format!("{error:#}");
            return DoctorCheck::fixable(
                check_id,
                title,
                detail,
                format!(
                    "Verify `ssh {destination}` succeeds noninteractively from this host, then install and start Docker Engine there; make sure `docker info` succeeds for the configured SSH user."
                ),
            );
        }
    };

    if smoke {
        let target = RuntimeTargetTemplate::SshDocker {
            ssh: ssh.clone(),
            container: RuntimeContainerTemplate {
                build_cache: None,
                image: image.to_owned(),
                pull_policy: Default::default(),
                extra_run_args: vec![],
                workspace_storage: Default::default(),
            },
        };
        return match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
            Ok(()) => DoctorCheck::ready(
                check_id,
                title,
                format!(
                    "Remote Docker {} is available via {destination}; disposable run/exec/remove and remote OverlayFS attachment smoke test passed for image {image}.",
                    preflight.version
                ),
            ),
            Err(error) => DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "Disposable run/exec/remove smoke test failed for image {image} on {destination}: {error:#}"
                ),
                format!(
                    "Fix the configured image or Docker runtime on {destination}, then run `mj doctor --json --smoke` again."
                ),
            ),
        };
    }

    let image_command = ssh_command(
        ssh,
        [
            "docker".to_owned(),
            "image".to_owned(),
            "inspect".to_owned(),
            image.to_owned(),
        ]
        .to_vec(),
    )
    .purpose("check remote Docker image presence");
    match executor.execute(&image_command) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            check_id,
            title,
            format!(
                "Remote Docker {} is available via {destination}; image {image} is present. Run `mj doctor --json --smoke` to verify remote OverlayFS attachments.",
                preflight.version
            ),
        ),
        Ok(output) => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Image {image} is not present in remote Docker storage on {destination}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            format!(
                "Pull it on {destination} with `ssh {destination} docker pull {image}`, or run `mj doctor --json --smoke`."
            ),
        ),
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Could not inspect remote Docker image {image} on {destination}: {error}"),
            format!(
                "Verify `ssh {destination} docker info` succeeds, then pull {image} on that host."
            ),
        ),
    }
}

/// Shared disposable-container identity for every doctor smoke test.
fn doctor_smoke_id() -> String {
    format!(
        "doctor-{}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn podman_remediation(error: &anyhow::Error) -> String {
    let fix = podman_remediation_match(error).unwrap_or(
        "Install Podman with `sudo apt update && sudo apt install -y podman uidmap` (Debian/Ubuntu) or `sudo dnf install -y podman shadow-utils` (Fedora).",
    );
    format!("{fix} See {PODMAN_DOCUMENTATION_URL}.")
}

/// A Podman failure without its fix, which the check reports separately.
fn podman_failure_detail(error: &anyhow::Error) -> String {
    podman_probe_observation(error).map_or_else(|| format!("{error:#}"), str::to_owned)
}

/// Map a Podman preflight failure to its specific remediation, if one applies.
///
/// The preflight reports which postcondition failed on the error itself, so
/// the fix is chosen from that probe rather than by matching the message text
/// this repository just produced. A failure that is not a probe result, such
/// as an unreachable SSH host, has no specific fix here.
fn podman_remediation_match(error: &anyhow::Error) -> Option<&'static str> {
    failed_podman_probe(error).map(PodmanProbe::remediation)
}

const AWS_CLI_INSTALL_URL: &str =
    "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html";

/// One check per `aws-ec2` target: the AWS CLI, its credentials, and the
/// configured launch template.
fn aws_checks(config: ConfigStatus<'_>, executor: &impl CommandExecutor) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                ..
            } => Some(aws_target_check(
                id,
                aws_profile.as_deref(),
                region,
                launch_template,
                executor,
            )),
            _ => None,
        })
        .collect()
}

/// The profile and region every AWS probe carries, applied exactly the way
/// provisioning applies them in `targets`.
fn aws_global_args<'a>(profile: Option<&'a str>, region: &'a str) -> Vec<String> {
    vec![
        "--profile".to_owned(),
        profile.unwrap_or("default").to_owned(),
        "--region".to_owned(),
        region.to_owned(),
    ]
}

fn aws_target_check(
    id: &str,
    profile: Option<&str>,
    region: &str,
    launch_template: &str,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let check_id = format!("runtime.aws-ec2.{id}");
    let title = format!("AWS EC2 target {id}");
    let profile_label = profile.unwrap_or("default");

    let version = CommandSpec::new("aws", ["--version"]).purpose("check AWS CLI installation");
    match executor.execute(&version) {
        Err(error) => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!("The `aws` command is not available: {error}"),
                format!("Install the AWS CLI and put `aws` on PATH: {AWS_CLI_INSTALL_URL}"),
            );
        }
        Ok(output) if output.status != 0 => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "`aws --version` failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                format!("Reinstall the AWS CLI: {AWS_CLI_INSTALL_URL}"),
            );
        }
        Ok(_) => {}
    }

    let mut identity_args = aws_global_args(profile, region);
    identity_args.extend(["sts".to_owned(), "get-caller-identity".to_owned()]);
    identity_args.extend(["--output".to_owned(), "json".to_owned()]);
    let identity =
        CommandSpec::new("aws", identity_args).purpose("check AWS credentials for a doctor target");
    match executor.execute(&identity) {
        Err(error) => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!("Could not run `aws sts get-caller-identity`: {error}"),
                format!(
                    "Configure credentials with `aws configure --profile {profile_label}`, or sign in with `aws sso login --profile {profile_label}`."
                ),
            );
        }
        Ok(output) if output.status != 0 => {
            return DoctorCheck::fixable(
                check_id,
                title,
                format!(
                    "AWS credentials for profile {profile_label} are not usable: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                format!(
                    "Configure credentials with `aws configure --profile {profile_label}`, or sign in with `aws sso login --profile {profile_label}`."
                ),
            );
        }
        Ok(_) => {}
    }

    // Launch templates are addressed by id when they carry the `lt-` prefix
    // and by name otherwise, the same split provisioning uses.
    let by_id = launch_template.starts_with("lt-");
    let mut template_args = aws_global_args(profile, region);
    template_args.extend(["ec2".to_owned(), "describe-launch-templates".to_owned()]);
    template_args.extend([
        if by_id {
            "--launch-template-ids".to_owned()
        } else {
            "--launch-template-names".to_owned()
        },
        launch_template.to_owned(),
    ]);
    template_args.extend(["--output".to_owned(), "json".to_owned()]);
    let template =
        CommandSpec::new("aws", template_args).purpose("check the configured AWS launch template");
    let template_remediation = format!(
        "Create the launch template in {region}, or point this target at an existing one; `aws --profile {profile_label} --region {region} ec2 describe-launch-templates` lists them."
    );
    match executor.execute(&template) {
        Err(error) => DoctorCheck::fixable(
            check_id,
            title,
            format!("Could not query launch template {launch_template}: {error}"),
            template_remediation,
        ),
        Ok(output) if output.status != 0 => DoctorCheck::fixable(
            check_id,
            title,
            format!(
                "Launch template {launch_template} was not found in {region}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            template_remediation,
        ),
        Ok(_) => DoctorCheck::ready(
            check_id,
            title,
            format!(
                "The AWS CLI is installed, profile {profile_label} has valid credentials, and launch template {launch_template} exists in {region}."
            ),
        ),
    }
}

/// Whether the running daemon is this build.
///
/// Two Mjolnir builds carry the same version string, so the version line in
/// `mj daemon status` cannot answer it. A daemon left over from before a
/// rebuild keeps serving the old code, and, when its executable was unlinked
/// by the rebuild, also loses every portable worker source it would have
/// pinned. Both are invisible without this check.
fn daemon_build_check() -> DoctorCheck {
    const ID: &str = "daemon.build";
    const TITLE: &str = "Daemon build";
    let Ok(metadata) = mj_client::daemon::read_metadata_any() else {
        return DoctorCheck::ready(
            ID,
            TITLE,
            "No Mjolnir daemon is running; the next command starts one from this build.",
        );
    };
    let pid = metadata.pid;
    match mj_client::executable::process_runs_this_executable(pid) {
        Ok(Some(true)) => DoctorCheck::ready(
            ID,
            TITLE,
            format!(
                "Daemon {pid} runs this build (version {}).",
                metadata.build_version
            ),
        ),
        Ok(Some(false)) => DoctorCheck::warning(
            ID,
            TITLE,
            format!(
                "{}. Two builds can report the same version, so the files are what tell them apart. Code rebuilt since that daemon started is not running.",
                mj_client::executable::describe_running_daemon_and_client_builds(
                    pid,
                    &metadata.build_version,
                ),
            ),
            "Run `mj daemon restart` from this build. It now fails rather than reporting success if another client's build wins.",
        ),
        Ok(None) => DoctorCheck::ready(
            ID,
            TITLE,
            format!(
                "Daemon {pid} is recorded but not running; the next command starts one from this build."
            ),
        ),
        Err(error) => DoctorCheck::warning(
            ID,
            TITLE,
            format!("Could not tell which build daemon {pid} runs: {error:#}"),
            "Run `mj daemon restart` from this build if rebuilt code is not taking effect.",
        ),
    }
}

/// Whether a new session would run the worker binary as it is on disk now.
///
/// The daemon copies each worker it can find into a content-addressed cache
/// when it starts and serves that copy for the rest of its life, so rebuilding
/// `mj-worker` does not reach a running daemon. Nothing else reports this, and
/// the digests are what make it checkable at all: two worker builds differ by
/// content, not by name or version.
fn worker_freshness_checks(config: ConfigStatus<'_>) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    let daemon = mj_client::daemon::read_metadata_any()
        .ok()
        .filter(|metadata| mj_client::daemon::process_is_alive(metadata.pid));
    let pinned = pinned_worker_digests();
    let mut sources: Vec<(String, Result<WorkerBinaryAvailability>)> = vec![(
        "this host".to_owned(),
        crate::controller::native_worker_binary_prerequisite(),
    )];
    for arch in container_worker_architectures(config) {
        sources.push((
            format!("{arch} Linux targets"),
            worker_binary_prerequisite_for_arch(&arch),
        ));
    }
    for (label, availability) in sources {
        let id = format!("worker.freshness.{}", label.replace(' ', "-"));
        let title = format!("Worker binary for {label}");
        let path = match availability {
            Ok(WorkerBinaryAvailability::Local { path, .. }) => path,
            // A remote worker is fetched by digest when a target is
            // provisioned, so it cannot go stale behind a running daemon.
            Ok(WorkerBinaryAvailability::Remote { .. }) => continue,
            Err(error) => {
                checks.push(DoctorCheck::unsupported(
                    id,
                    title,
                    format!("No worker binary resolves for {label}: {error:#}"),
                ));
                continue;
            }
        };
        let digest = mj_core::worker_launch::worker_executable_digest(&path)
            .unwrap_or_else(|error| format!("unreadable ({error:#})"));
        let pinned_note = if pinned.is_empty() {
            "the daemon has pinned no worker".to_owned()
        } else if pinned.contains(&digest) {
            "this content is in the daemon's pinned worker cache".to_owned()
        } else {
            format!(
                "the pinned worker cache holds {} instead",
                pinned.join(", ")
            )
        };
        let detail = format!("{} has digest {digest}; {pinned_note}.", path.display());
        let Some(metadata) = daemon.as_ref() else {
            checks.push(DoctorCheck::ready(
                id,
                title,
                format!("{detail} No daemon is running, so the next session uses this file."),
            ));
            continue;
        };
        match worker_changed_since_daemon_start(&path, &metadata.started_at) {
            Ok(true) => checks.push(DoctorCheck::warning(
                id,
                title,
                format!("{detail} It was rebuilt after daemon {} started, which froze the copy it serves.", metadata.pid),
                "Run `mj daemon restart` so new sessions use the rebuilt worker. Sessions already running keep their worker until they are quiet enough to be upgraded.",
            )),
            Ok(false) => checks.push(DoctorCheck::ready(id, title, detail)),
            Err(error) => checks.push(DoctorCheck::warning(
                id,
                title,
                format!("{detail} Could not compare it with the daemon's start time: {error:#}"),
                "Run `mj daemon restart` if rebuilt worker code is not taking effect.",
            )),
        }
    }
    checks
}

/// The architectures this configuration needs a portable Linux worker for.
fn container_worker_architectures(config: ConfigStatus<'_>) -> Vec<String> {
    let Ok(config) = config else {
        return Vec::new();
    };
    let mut architectures = Vec::new();
    for target in config.targets.values() {
        let container = match target {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::AppleContainer { container }
            | TargetTemplate::SshPodman { container, .. }
            | TargetTemplate::SshDocker { container, .. } => container,
            _ => continue,
        };
        let arch = container
            .platform
            .as_deref()
            .and_then(|platform| platform.rsplit('/').next())
            .map_or_else(
                || std::env::consts::ARCH.to_owned(),
                normalized_worker_architecture,
            );
        if !architectures.contains(&arch) {
            architectures.push(arch);
        }
    }
    architectures
}

/// Container platforms name architectures the way Docker does; worker files
/// are named the way Rust target triples do.
fn normalized_worker_architecture(platform_arch: &str) -> String {
    match platform_arch {
        "amd64" => "x86_64".to_owned(),
        "arm64" => "aarch64".to_owned(),
        other => other.to_owned(),
    }
}

/// The digests the daemon's immutable worker cache holds.
///
/// The cache is never pruned, so this is what any daemon on this machine has
/// pinned at some point, which is why it is reported rather than judged.
fn pinned_worker_digests() -> Vec<String> {
    let root = mj_core::config::data_dir().join("workers").join("pinned");
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut digests: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    digests.sort();
    digests
}

/// Whether a worker file was written after the daemon started.
///
/// The daemon copies the file it finds at startup, so a later modification is
/// exactly the case where the running daemon serves older content.
fn worker_changed_since_daemon_start(path: &Path, started_at: &str) -> Result<bool> {
    let started: SystemTime = chrono::DateTime::parse_from_rfc3339(started_at)
        .map_err(|error| anyhow::anyhow!("parse daemon start time {started_at:?}: {error}"))?
        .into();
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(modified > started)
}

fn worker_binary_checks(config: ConfigStatus<'_>) -> Vec<DoctorCheck> {
    let config = match config {
        Ok(config) => config,
        Err(ConfigGap::NewerVersion(version)) => {
            return vec![newer_config_skip(
                "worker.containers",
                "Container worker binary",
                version,
            )];
        }
        Err(ConfigGap::Unreadable) => {
            return vec![DoctorCheck::fixable(
                "worker.containers",
                "Container worker binary",
                "Worker availability cannot be checked until config.toml is valid.",
                "Fix config.toml, then rerun `mj doctor --json`.",
            )];
        }
    };
    let containers = config
        .targets
        .iter()
        .filter_map(|(id, target)| match target {
            TargetTemplate::LocalPodman { container }
            | TargetTemplate::LocalDocker { container }
            | TargetTemplate::AppleContainer { container } => Some((id, container, None)),
            TargetTemplate::SshPodman { container, .. } => {
                Some((id, container, Some("ssh-podman")))
            }
            TargetTemplate::SshDocker { container, .. } => {
                Some((id, container, Some("ssh-docker")))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if containers.is_empty() {
        return vec![DoctorCheck::unsupported(
            "worker.containers",
            "Container worker binary",
            "No container target is configured.",
        )];
    }
    containers
        .into_iter()
        .map(|(id, container, remote_kind)| {
            if let Some(remote_kind) = remote_kind
                && container.platform.is_none()
            {
                // The remote CPU architecture is only observable once the host
                // is reachable, so an explicit `platform` is required here.
                return DoctorCheck::unsupported(
                    format!("worker.{id}"),
                    format!("Container worker binary for target {id}"),
                    format!(
                        "Set `platform` on this {remote_kind} target to check its worker binary; the remote architecture is unknown until provisioning."
                    ),
                );
            }
            worker_binary_check(id, container)
        })
        .collect()
}

fn worker_binary_check(id: &str, container: &ContainerTemplate) -> DoctorCheck {
    let title = format!("Container worker binary for target {id}");
    let arch = match container_architecture(container.platform.as_deref()) {
        Ok(arch) => arch,
        Err(reason) => {
            return DoctorCheck::unsupported(format!("worker.{id}"), title, reason);
        }
    };
    let triple = format!("{arch}-unknown-linux-musl");
    match worker_binary_prerequisite_for_arch(arch) {
        Ok(WorkerBinaryAvailability::Local { path, source }) => DoctorCheck::ready(
            format!("worker.{id}"),
            title,
            format!(
                "{triple} worker is available from {source}: {}",
                path.display()
            ),
        ),
        Ok(WorkerBinaryAvailability::Remote { url, .. }) => DoctorCheck::ready(
            format!("worker.{id}"),
            title,
            format!("{triple} worker will be verified and downloaded from {url} when needed."),
        ),
        Err(error) => DoctorCheck::fixable(
            format!("worker.{id}"),
            title,
            format!("No usable {triple} worker source: {error:#}"),
            format!(
                "Build it with `cargo build --release --target {triple} -p brokk-mj-worker --bin mj-worker`, install `mj-worker-{triple}` beside `mj`, or set MJ_WORKER_BINARY, MJ_WORKER_DIR, or MJ_WORKER_URL with MJ_WORKER_SHA256."
            ),
        ),
    }
}

fn container_architecture(platform: Option<&str>) -> std::result::Result<&'static str, String> {
    let candidate = platform.unwrap_or(std::env::consts::ARCH);
    let candidate = candidate
        .split('/')
        .rev()
        .find(|part| matches!(*part, "x86_64" | "amd64" | "aarch64" | "arm64"))
        .unwrap_or(candidate);
    match candidate {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        other => Err(format!(
            "Container architecture {other:?} is unsupported; Mjolnir supports x86_64 and aarch64 Linux workers."
        )),
    }
}

fn apple_container_image(config: ConfigStatus<'_>) -> String {
    config
        .ok()
        .and_then(|config| {
            config.targets.values().find_map(|target| match target {
                TargetTemplate::AppleContainer { container } => Some(container.image.clone()),
                _ => None,
            })
        })
        .unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.into())
}

pub fn apple_container_check(
    platform: &ApplePlatform,
    executor: &impl CommandExecutor,
    smoke: bool,
    image: String,
) -> DoctorCheck {
    match platform {
        ApplePlatform::Linux => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                "macOS only",
            );
        }
        ApplePlatform::Other(current) => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                format!("macOS only (current platform: {current})"),
            );
        }
        ApplePlatform::Macos {
            architecture,
            major_version,
        } if architecture != "aarch64" && architecture != "arm64" => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                "Apple container requires Apple silicon; Intel Macs are unsupported.",
            );
        }
        ApplePlatform::Macos { major_version, .. } if *major_version < 26 => {
            return DoctorCheck::unsupported(
                "runtime.apple-container",
                "Apple container runtime",
                format!("Apple container requires macOS 26 or newer (found {major_version})."),
            );
        }
        ApplePlatform::Macos { .. } => {}
    }

    let daemon = apple_container_daemon_check(executor);
    if daemon.status != CheckStatus::Ready {
        return daemon;
    }

    if !smoke {
        return DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            "The daemon is running, but the required disposable smoke test was not requested.",
            "Run `mj doctor --json --smoke`.",
        );
    }

    let target = RuntimeTargetTemplate::AppleContainer(RuntimeContainerTemplate {
        build_cache: None,
        image,
        pull_policy: Default::default(),
        extra_run_args: vec![],
        workspace_storage: Default::default(),
    });
    match run_setup_smoke_test(&target, &doctor_smoke_id(), executor) {
        Ok(()) => DoctorCheck::ready(
            "runtime.apple-container",
            "Apple container runtime",
            "Installed, daemon running, and disposable run/exec/remove smoke test passed.",
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            format!("Disposable run/exec/remove smoke test failed: {error:#}"),
            "Fix the configured image or container runtime, then run `mj doctor --json --smoke` again.",
        ),
    }
}

/// Probe that the Apple `container` command is installed and its daemon is
/// running, phrased as a doctor check.
///
/// Split out of [`apple_container_check`] so `mj setup` can reuse the same
/// probes and remediation text without also demanding the opt-in smoke test.
/// The caller is responsible for platform gating.
pub fn apple_container_daemon_check(executor: &impl CommandExecutor) -> DoctorCheck {
    let installed =
        CommandSpec::new("container", ["--version"]).purpose("check Apple container installation");
    match executor.execute(&installed) {
        Err(error) => {
            return DoctorCheck::fixable(
                "runtime.apple-container",
                "Apple container runtime",
                format!("The `container` command is not available: {error}"),
                format!("Install the official signed package: {APPLE_CONTAINER_INSTALL_URL}"),
            );
        }
        Ok(output) if output.status != 0 => {
            return DoctorCheck::fixable(
                "runtime.apple-container",
                "Apple container runtime",
                format!(
                    "The installed `container --version` command failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                format!("Reinstall the official signed package: {APPLE_CONTAINER_INSTALL_URL}"),
            );
        }
        Ok(_) => {}
    }

    let status =
        CommandSpec::new("container", ["system", "status"]).purpose("check Apple container daemon");
    match executor.execute(&status) {
        Ok(output) if output.status == 0 => DoctorCheck::ready(
            "runtime.apple-container",
            "Apple container runtime",
            "Installed, and the Apple container daemon is running.",
        ),
        Ok(output) => DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            format!(
                "The Apple container daemon is stopped: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            "Run `container system start`.",
        ),
        Err(error) => DoctorCheck::fixable(
            "runtime.apple-container",
            "Apple container runtime",
            format!("Could not query the Apple container daemon: {error}"),
            "Run `container system start`.",
        ),
    }
}

pub fn current_apple_platform(executor: &impl CommandExecutor) -> ApplePlatform {
    if cfg!(target_os = "linux") {
        return ApplePlatform::Linux;
    }
    if !cfg!(target_os = "macos") {
        return ApplePlatform::Other(std::env::consts::OS.into());
    }
    let major_version = executor
        .execute(&CommandSpec::new("sw_vers", ["-productVersion"]).purpose("detect macOS version"))
        .ok()
        .filter(|output| output.status == 0)
        .and_then(|output| {
            String::from_utf8(output.stdout)
                .ok()
                .and_then(|value| value.trim().split('.').next()?.parse().ok())
        })
        .unwrap_or(0);
    ApplePlatform::Macos {
        architecture: std::env::consts::ARCH.into(),
        major_version,
    }
}

#[cfg(test)]
mod tests;

/// What one repository still holds from a Mjolnir review capture.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewResidue {
    /// `refs/hel/*` refs in the repository.
    pub refs: Vec<String>,
    /// Scratch index files left in the Git directory by an interrupted capture.
    pub scratch_indexes: Vec<PathBuf>,
}

impl ReviewResidue {
    fn is_empty(&self) -> bool {
        self.refs.is_empty() && self.scratch_indexes.is_empty()
    }
}

/// Read what a repository still holds from Mjolnir's review captures.
///
/// Releases before this one staged the whole working tree into the user's own
/// object store and pinned it with two refs, and a capture that was killed
/// partway left its scratch index behind. Both are the user's to remove, so
/// this only reads.
pub(crate) fn review_residue(repository: &Path) -> ReviewResidue {
    let mut residue = ReviewResidue::default();
    let git_dir = repository.join(".git");
    if !git_dir.exists() {
        return residue;
    }
    for reference in ["review-baseline", "review-capture"] {
        if git_dir.join("refs/hel").join(reference).is_file() {
            residue.refs.push(format!("refs/hel/{reference}"));
        }
    }
    // A packed ref survives `git pack-refs`, which a `git gc` runs.
    if let Ok(packed) = std::fs::read_to_string(git_dir.join("packed-refs")) {
        for line in packed.lines() {
            if let Some((_, reference)) = line.split_once(' ')
                && reference.starts_with("refs/hel/")
                && !residue.refs.iter().any(|known| known == reference)
            {
                residue.refs.push(reference.to_owned());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(&git_dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("hel-review-index-"))
            {
                residue.scratch_indexes.push(entry.path());
            }
        }
    }
    residue.refs.sort();
    residue.scratch_indexes.sort();
    residue
}

/// Report Mjolnir's own leftovers in the repositories the configuration names.
///
/// This deletes nothing. Removing refs and running `git gc` in someone else's
/// repository without asking is the same mistake as writing to it without
/// asking, which is what left this residue in the first place.
fn review_residue_checks(config: ConfigStatus<'_>) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    let mut repositories: Vec<PathBuf> = config
        .bundles
        .values()
        .flat_map(|bundle| bundle.repositories.iter())
        .filter_map(|repository| repository.local.clone())
        .collect();
    // A session started with `--project-directory` has no bundle, and those
    // are exactly the repositories a person works in by hand, so they are the
    // ones where leftovers matter most. A daemon-less machine has no session
    // database, which is not a reason to skip the configured repositories.
    if let Ok(state) = crate::database::load_state() {
        repositories.extend(
            state
                .sessions
                .values()
                .filter_map(|session| session.project_directory.clone()),
        );
    }
    repositories.sort();
    repositories.dedup();
    if repositories.is_empty() {
        return Vec::new();
    }
    let found = repositories
        .into_iter()
        .map(|repository| {
            let residue = review_residue(&repository);
            (repository, residue)
        })
        .filter(|(_, residue)| !residue.is_empty())
        .collect::<Vec<_>>();
    if found.is_empty() {
        return vec![DoctorCheck::ready(
            "review.residue",
            "Review leftovers in your repositories",
            "No Mjolnir refs or scratch index files were found in the configured repositories.",
        )];
    }
    let detail = found
        .iter()
        .map(|(repository, residue)| {
            let mut parts = Vec::new();
            if !residue.refs.is_empty() {
                parts.push(residue.refs.join(", "));
            }
            if !residue.scratch_indexes.is_empty() {
                parts.push(format!(
                    "{} leftover scratch index file(s)",
                    residue.scratch_indexes.len()
                ));
            }
            format!("{}: {}", repository.display(), parts.join("; "))
        })
        .collect::<Vec<_>>()
        .join(". ");
    let commands = found
        .iter()
        .flat_map(|(repository, residue)| {
            let repository = repository.display().to_string();
            let mut commands = residue
                .refs
                .iter()
                .map(|reference| format!("git -C {repository} update-ref -d {reference}"))
                .collect::<Vec<_>>();
            commands.extend(
                residue
                    .scratch_indexes
                    .iter()
                    .map(|index| format!("rm -f {}", index.display())),
            );
            commands.push(format!("git -C {repository} gc --prune=now"));
            commands
        })
        .collect::<Vec<_>>()
        .join("\n");
    vec![DoctorCheck::fixable(
        "review.residue",
        "Review leftovers in your repositories",
        format!(
            "Mjolnir left these in repositories it does not own: {detail}. \
             A running session's own `refs/hel/review-baseline` is in use; \
             remove that one only when no session is working in that repository."
        ),
        format!("Remove them yourself when you are ready:\n{commands}"),
    )]
}

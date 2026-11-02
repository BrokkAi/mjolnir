//! Additive first-run discovery shared by the dashboard and `mj setup`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use anyhow::Context;

use crate::doctor::{
    ApplePlatform, CheckStatus, apple_container_runtime_check, current_apple_platform,
    local_docker_runtime_check, local_podman_runtime_check,
};
use crate::targets::{CommandExecutor, CommandSpec};
use mj_core::config::{
    Config, ContainerTemplate, HarnessHost, HarnessKind, HarnessProfile, ProjectBundle,
    ProjectRepository, TargetTemplate, unique_config_id as unique_id,
};

mod availability;
pub use availability::{AvailabilityReport, check_availability, profile_presence};
mod machines;
#[cfg(test)]
pub(crate) use machines::machine_probe_fixture;
pub use machines::{
    MachineProbe, MachineProbeOutcome, add_machine_runtimes, probe_machine, probe_machines,
};
mod startup;
pub use startup::{
    SetupReport, actionable_errors, add_detected_profiles, run_setup, run_setup_command,
};

// Published from containers/Containerfile.agent-dev by
// .github/workflows/publish-agent-dev-image.yml. It already carries Node, Rust,
// Git, gh, and the pinned ACP bridges, so a first session does not have to
// install them.
pub use mj_client::target::DEFAULT_IMAGE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHome {
    pub kind: HarnessKind,
    pub path: PathBuf,
    /// A Codex profile overlay in this home, `<path>/<name>.config.toml`.
    /// Every overlay is offered as a profile of its own beside the home.
    pub codex_profile: Option<String>,
    pub authenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepository {
    pub owner: String,
    pub repository: String,
}

impl GithubRepository {
    fn source(&self) -> String {
        format!("{}/{}", self.owner, self.repository)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// The harness runs directly on the machine, under a worker.
    Bare,
    Podman,
    Docker,
    AppleContainer,
}

impl RuntimeKind {
    /// The kind as a runtime stores it, as `runtime_kind_name` answers for a
    /// configured runtime.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bare => "bare",
            Self::Podman => "podman",
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bare => "Bare",
            Self::Podman => "Podman",
            Self::Docker => "Docker",
            Self::AppleContainer => "Apple container",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProbe {
    pub kind: RuntimeKind,
    pub status: CheckStatus,
    pub detail: String,
    /// The fix `mj doctor` would print for this runtime, carried through so
    /// setup never invents its own remediation wording.
    pub remediation: Option<String>,
}

impl RuntimeProbe {
    pub fn usable(&self) -> bool {
        self.status == CheckStatus::Ready
    }
}

/// The installed agent homes alone. Callers that only add profiles use this
/// without probing unrelated runtimes or remote targets.
pub fn discover_profiles(executor: &impl CommandExecutor) -> Vec<DiscoveredHome> {
    let home = mj_core::path_input::user_home();
    let overrides = HarnessKind::ALL
        .into_iter()
        .filter_map(|kind| {
            std::env::var_os(kind.home_env()).map(|path| (kind, kind.home_from_environment(path)))
        })
        .collect::<BTreeMap<_, _>>();
    let mut homes =
        discover_harness_homes_with_executor(home.as_deref(), overrides.clone(), executor);
    discover_installed_harnesses(home.as_deref(), &overrides, &mut homes, executor, |key| {
        std::env::var(key).is_ok_and(|value| !value.trim().is_empty())
    });
    homes
}

/// The container runtimes on this machine alone, usable or not, so a caller
/// can report why an unusable one was skipped.
pub fn discover_runtimes(executor: &impl CommandExecutor) -> Vec<RuntimeProbe> {
    probe_local_runtimes(executor, &current_apple_platform(executor))
}

/// A configuration holding the discovered profiles and nothing else, for
/// merging into an existing configuration.
pub fn profiles_config(homes: &[DiscoveredHome]) -> Config {
    build_config(homes, None)
}

/// Point each discovered profile of a harness that reads an API key (DSH's
/// `DEEPSEEK_API_KEY`) at the secret of that name, when `secrets.toml` beside
/// `config_path` defines it. A key exported in Mjolnir's environment needs no
/// entry, but one kept only in the secrets file reaches the harness only
/// through `KEY = { from_secret = "KEY" }`, and without it the detected
/// profile reports itself unauthenticated.
pub fn reference_secret_api_keys(discovered: &mut Config, config_path: &Path) {
    let Ok(secrets) =
        mj_core::config::load_secrets(&mj_core::config::secrets_path_beside(config_path))
    else {
        return;
    };
    for profile in discovered.profiles.values_mut() {
        let Some(key) = profile.kind.api_key_variable() else {
            continue;
        };
        if profile.environment.sources().contains_key(key)
            || !secrets
                .get(key)
                .is_some_and(|value| !value.trim().is_empty())
        {
            continue;
        }
        let mut sources = profile.environment.sources().clone();
        sources.insert(
            key.to_owned(),
            mj_core::config::EnvironmentValue::FromSecret(key.to_owned()),
        );
        // Resolve as a load of the saved profile will, so discovery reports
        // the key the profile will run on, such as one saved in DSH.
        mj_core::config::with_secret_resolver(
            mj_core::config::SecretResolver::beside(config_path),
            || {
                profile.environment = mj_core::config::Environment::from_sources(sources);
                profile.inherit_provider_key();
            },
        );
    }
}

/// Read the concrete `Host` aliases from `~/.ssh/config`.
///
/// This is a pure read: setup never runs `ssh` while discovering. `Include`
/// directives are deliberately not followed, because resolving them correctly
/// means reimplementing OpenSSH's glob and relative-path rules; aliases that
/// live in an included file simply are not offered, and the user can still
/// type a host by hand.
pub fn discover_ssh_hosts(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(home.join(".ssh").join("config")) else {
        return Vec::new();
    };
    ssh_config_aliases(&contents)
}

/// Extract the usable `Host` aliases from SSH config text.
///
/// Pattern entries (`*`, `?`, `!`) are skipped: they configure other hosts
/// rather than naming one Hel could connect to.
pub fn ssh_config_aliases(contents: &str) -> Vec<String> {
    let mut aliases: Vec<String> = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((keyword, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for alias in rest.split_whitespace() {
            let alias = alias.trim_matches('"');
            if alias.is_empty() || alias.contains(['*', '?', '!']) {
                continue;
            }
            if !aliases.iter().any(|existing| existing == alias) {
                aliases.push(alias.to_owned());
            }
        }
    }
    aliases
}

/// A newly installed CLI may not create its profile directory until login,
/// and a harness that authenticates with an API key needs no CLI at all: the
/// key in the environment Mjolnir started with is its installation signal.
/// Run these probes through setup's bounded, cancellable executor.
fn discover_installed_harnesses(
    user_home: Option<&Path>,
    overrides: &BTreeMap<HarnessKind, PathBuf>,
    homes: &mut Vec<DiscoveredHome>,
    executor: &impl CommandExecutor,
    api_key_is_set: impl Fn(&str) -> bool,
) {
    for kind in HarnessKind::ALL {
        if homes.iter().any(|home| home.kind == kind) {
            continue;
        }
        let Some(home) = overrides
            .get(&kind)
            .cloned()
            .or_else(|| user_home.map(|home| home.join(kind.default_home_leaf())))
        else {
            continue;
        };
        if let Some(env_key) = kind.api_key_variable()
            && api_key_is_set(env_key)
        {
            homes.push(DiscoveredHome {
                kind,
                path: home,
                codex_profile: None,
                authenticated: true,
            });
            continue;
        }
        if harness_cli_installed(kind, executor) == Some(true) {
            homes.push(DiscoveredHome {
                kind,
                path: home,
                codex_profile: None,
                authenticated: false,
            });
        }
    }
}

/// Whether the harness's own command answers on this machine, or `None`
/// when the probe could not run (cancelled, timed out). Discovery offers a
/// profile for an installed command before its first login, and a
/// configured profile without a home is still present while it does.
pub(crate) fn harness_cli_installed(
    kind: HarnessKind,
    executor: &impl CommandExecutor,
) -> Option<bool> {
    let probe = CommandSpec::new(kind.cli_binary_name(), ["--version"])
        .purpose("detect installed harness before first login");
    match executor.execute(&probe) {
        Ok(output) => Some(output.status == 0),
        Err(error) if crate::controller::is_missing_command(&error) => Some(false),
        Err(error) => {
            tracing::debug!(
                harness = kind.id(),
                "installation probe unavailable: {error:#}"
            );
            None
        }
    }
}

pub(crate) fn discover_harness_homes_with_executor(
    home: Option<&Path>,
    overrides: impl IntoIterator<Item = (HarnessKind, PathBuf)>,
    executor: &impl CommandExecutor,
) -> Vec<DiscoveredHome> {
    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.extend(
            HarnessKind::ALL
                .into_iter()
                .map(|kind| (kind, home.join(kind.default_home_leaf()), true)),
        );
    }
    candidates.extend(
        overrides
            .into_iter()
            .map(|(kind, path)| (kind, path, false)),
    );

    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|(kind, path, _)| seen.insert((*kind, path.clone())) && path.is_dir())
        .flat_map(|(kind, path, is_default_home)| {
            let overlays = match kind {
                HarnessKind::Codex => mj_core::codex_provider::overlay_names(&path),
                _ => Vec::new(),
            };
            std::iter::once(None)
                .chain(overlays.into_iter().map(Some))
                .map(move |codex_profile| {
                    let profile = probe_profile(kind, &path, codex_profile.clone());
                    DiscoveredHome {
                        authenticated: harness_is_authenticated_with(
                            &profile,
                            is_default_home,
                            executor,
                        ),
                        kind,
                        path: path.clone(),
                        codex_profile,
                    }
                })
        })
        .collect()
}

/// A profile standing in for a home discovery found but the user has not
/// configured. It carries only the provider keys a configured profile would
/// inherit, so cloud backends must be configured explicitly before their
/// authentication can be recognized.
fn probe_profile(kind: HarnessKind, home: &Path, codex_profile: Option<String>) -> HarnessProfile {
    let mut profile = HarnessProfile {
        enabled: true,
        kind,
        home: home.to_path_buf(),
        environment: Default::default(),
        context_window_bytes: None,
        legacy_subagents: Default::default(),
        guardian_review_model: None,
        muse_auto_review: None,
        codex_profile,
    };
    profile.inherit_provider_key();
    profile
}

pub(crate) fn harness_is_authenticated_with_executor(
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> bool {
    let is_default_home = mj_core::path_input::user_home()
        .is_some_and(|user_home| profile.home == user_home.join(profile.kind.default_home_leaf()));
    harness_is_authenticated_with(profile, is_default_home, executor)
}

/// Whether this profile can talk to its service without a login first.
///
/// An API-key profile is proven by its harness configuration file, because its
/// key lives in the profile's `environment` rather than in a credential file;
/// [`HarnessProfile::authentication_marker`] already names the right file for
/// either case. A harness without a login or a configuration file, such as
/// DSH, is proven by its key alone.
fn harness_is_authenticated_with(
    profile: &HarnessProfile,
    is_default_home: bool,
    executor: &impl CommandExecutor,
) -> bool {
    let kind = profile.kind;
    let home = profile.home.as_path();
    // AWS credentials can come from a role or an external credential process;
    // there is no harness login file. Doctor probes the chain separately.
    if profile.auth_scheme() == mj_core::config::AuthScheme::AwsCredentialChain {
        return true;
    }
    // OpenCode keeps its logins in its own database, or takes a provider key
    // from its environment; doctor reports a database it cannot read.
    if profile.auth_scheme() == mj_core::config::AuthScheme::NativeLoginStore {
        return crate::opencode_logins::profile_logins(profile)
            .is_ok_and(|logins| logins.is_some())
            || !profile.opencode_environment_logins().is_empty();
    }
    // A harness without a login reads its key from its environment, so no
    // file can prove it; the key itself does.
    if let Some(env_key) = kind.api_key_variable() {
        return profile.supplies_api_key(env_key);
    }
    if profile.authentication_marker().is_file()
        || (kind == HarnessKind::Kimi && home.join("credentials").is_file())
    {
        return true;
    }
    if kind != HarnessKind::Claude {
        return false;
    }
    // Where the login does not live in the home, every Claude profile shares
    // the one Keychain item, so asking the CLI about a scoped home would only
    // report the default profile's state again.
    if is_default_home || !kind.keeps_login_in_home(HarnessHost::current()) {
        return claude_keychain_reports_authenticated(executor);
    }
    claude_cli_reports_authenticated(home, executor)
}

/// Ask Claude Code about a scoped profile. Setting `CLAUDE_CONFIG_DIR` for the
/// default home changes Claude's profile selection, so the default macOS
/// profile is checked directly in the Keychain instead.
fn claude_cli_reports_authenticated(home: &Path, executor: &impl CommandExecutor) -> bool {
    let mut command = CommandSpec::new("claude", ["auth", "status", "--json"])
        .purpose("check Claude Code authentication");
    command.env.insert(
        HarnessKind::Claude.home_env().to_owned(),
        home.to_string_lossy().into_owned(),
    );
    let Ok(output) = executor.execute(&command) else {
        return false;
    };
    if output.status != 0 {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|status| status.get("loggedIn").and_then(serde_json::Value::as_bool))
        == Some(true)
}

/// Mjolnir and Claude Code use this service for the default macOS profile.
/// `security` is already authorized for the item, so this does not raise a
/// Keychain prompt; the shared executor still bounds a wedged lookup.
#[cfg(target_os = "macos")]
fn claude_keychain_reports_authenticated(executor: &impl CommandExecutor) -> bool {
    claude_keychain_credentials(executor)
        .ok()
        .flatten()
        .is_some_and(|credentials| claude_credentials_contain_login(&credentials))
}

/// Read Claude Code's shared macOS login for a target that cannot reach the
/// controller's Keychain. The caller owns the returned copy and must not log
/// it.
#[cfg(target_os = "macos")]
pub(crate) fn claude_keychain_credentials(
    executor: &impl CommandExecutor,
) -> anyhow::Result<Option<Vec<u8>>> {
    let command = CommandSpec::new(
        "security",
        [
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ],
    )
    .purpose("read Claude Code authentication from the macOS Keychain");
    let output = executor
        .execute(&command)
        .context("read Claude Code's login from the macOS Keychain")?;
    if output.status != 0 {
        return Ok(None);
    }
    if !claude_credentials_contain_login(&output.stdout) {
        anyhow::bail!(
            "the macOS Keychain item \"Claude Code-credentials\" does not contain a usable Claude login"
        );
    }
    Ok(Some(output.stdout))
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_reports_authenticated(_executor: &impl CommandExecutor) -> bool {
    false
}

#[cfg(any(target_os = "macos", test))]
fn claude_credentials_contain_login(credentials: &[u8]) -> bool {
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(credentials) else {
        return false;
    };
    [
        "/claudeAiOauth/accessToken",
        "/claudeAiOauth/refreshToken",
        "/oauth/accessToken",
        "/apiKey",
    ]
    .into_iter()
    .any(|pointer| {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

pub fn github_repository_from_origin(origin: &str) -> Option<GithubRepository> {
    let (owner, repository) = mj_core::remote_git::github_owner_repo(origin)?;
    Some(GithubRepository { owner, repository })
}

/// Read the current directory's GitHub origin, through the same executor every
/// other discovery probe uses so it is bounded and can be faked in tests.
///
/// `git -C` selects the directory instead of a working-directory field on the
/// command, which no executor carries.
fn discover_github_repository(
    executor: &impl CommandExecutor,
    cwd: &Path,
) -> Option<GithubRepository> {
    let command = CommandSpec::new(
        "git",
        [
            "-C".to_owned(),
            cwd.to_string_lossy().into_owned(),
            "remote".to_owned(),
            "get-url".to_owned(),
            "origin".to_owned(),
        ],
    )
    .purpose("detect the current repository's GitHub origin");
    let output = executor.execute(&command).ok()?;
    if output.status != 0 {
        return None;
    }
    github_repository_from_origin(&String::from_utf8_lossy(&output.stdout))
}

/// Probe the container runtimes setup can configure, reusing the doctor checks
/// so an unavailable runtime carries doctor's detail and remediation.
pub fn probe_local_runtimes(
    executor: &impl CommandExecutor,
    platform: &ApplePlatform,
) -> Vec<RuntimeProbe> {
    let mut probes = vec![
        runtime_probe_from_check(
            RuntimeKind::Podman,
            local_podman_runtime_check(executor, platform),
        ),
        runtime_probe_from_check(RuntimeKind::Docker, local_docker_runtime_check(executor)),
    ];
    if matches!(platform, ApplePlatform::Macos { .. }) {
        probes.push(runtime_probe_from_check(
            RuntimeKind::AppleContainer,
            apple_container_runtime_check(platform, executor),
        ));
    }
    probes
}

fn runtime_probe_from_check(kind: RuntimeKind, check: crate::doctor::DoctorCheck) -> RuntimeProbe {
    RuntimeProbe {
        kind,
        status: check.status,
        detail: check.detail,
        remediation: check.remediation,
    }
}

/// Configuration additions for installed profiles and the current repository.
pub fn build_config(homes: &[DiscoveredHome], repository: Option<&GithubRepository>) -> Config {
    let mut config = Config::default();
    for home in homes {
        let base = match &home.codex_profile {
            Some(name) => format!("{}-{name}", home.kind.id()),
            None => home.kind.id().to_owned(),
        };
        let id = unique_id(&config.profiles, &base);
        config.profiles.insert(
            id,
            HarnessProfile {
                enabled: true,
                kind: home.kind,
                home: home.path.clone(),
                environment: Default::default(),
                context_window_bytes: None,
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                muse_auto_review: None,
                codex_profile: home.codex_profile.clone(),
            },
        );
    }

    if let Some(repository) = repository {
        let repository_id = config_id(&repository.repository);
        config.bundles.insert(
            "current-repository".to_owned(),
            ProjectBundle {
                primary_repo: repository_id.clone(),
                repositories: vec![ProjectRepository {
                    id: repository_id.clone(),
                    github: Some(repository.source()),
                    local: None,
                    destination: PathBuf::from(repository_id),
                    git_ref: None,
                }],
            },
        );
    }

    #[cfg(unix)]
    config
        .targets
        .insert("localhost".to_owned(), TargetTemplate::LocalBare);
    config
}

/// The shared setup/startup template for a locally available container engine.
pub fn local_runtime_target(runtime: RuntimeKind, image: &str) -> (&'static str, TargetTemplate) {
    let container = ContainerTemplate {
        build_cache: None,
        image: image.trim().to_owned(),
        pull_policy: Default::default(),
        platform: None,
        cpus: None,
        memory: None,
        environment: Default::default(),
        workspace_storage: Default::default(),
    };
    match runtime {
        RuntimeKind::Bare => ("localhost", TargetTemplate::LocalBare),
        RuntimeKind::Podman => ("podman", TargetTemplate::LocalPodman { container }),
        RuntimeKind::Docker => ("docker", TargetTemplate::LocalDocker { container }),
        RuntimeKind::AppleContainer => (
            "apple-container",
            TargetTemplate::AppleContainer { container },
        ),
    }
}

fn config_id(value: &str) -> String {
    let mut id = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .take(64)
        .collect::<String>();
    if id.is_empty() || matches!(id.as_str(), "." | "..") {
        id = "repository".to_owned();
    }
    id
}

#[cfg(test)]
mod tests;

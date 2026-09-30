//! Additive first-run discovery shared by the dashboard and `mj setup`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::doctor::{
    ApplePlatform, CheckStatus, apple_container_runtime_check, current_apple_platform,
    local_docker_runtime_check, local_podman_runtime_check,
};
use crate::targets::{CommandExecutor, CommandSpec};
use mj_core::config::{
    Config, ContainerTemplate, HarnessHost, HarnessKind, HarnessProfile, ProjectBundle,
    ProjectRepository, TargetTemplate, unique_config_id as unique_id,
};

mod startup;
pub use startup::{SetupReport, actionable_errors, run_setup, run_setup_command};

// Published from containers/Containerfile.agent-dev by
// .github/workflows/publish-agent-dev-image.yml. It already carries Node, Rust,
// Git, gh, and the pinned ACP bridges, so a first session does not have to
// install them.
pub use mj_client::target::DEFAULT_IMAGE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHome {
    pub kind: HarnessKind,
    pub path: PathBuf,
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
    Podman,
    Docker,
    AppleContainer,
}

impl RuntimeKind {
    pub fn label(self) -> &'static str {
        match self {
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
    let home = dirs::home_dir();
    let overrides = HarnessKind::ALL
        .into_iter()
        .filter_map(|kind| {
            std::env::var_os(kind.home_env()).map(|path| (kind, kind.home_from_environment(path)))
        })
        .collect::<BTreeMap<_, _>>();
    let mut homes =
        discover_harness_homes_with_executor(home.as_deref(), overrides.clone(), executor);
    discover_installed_harnesses(home.as_deref(), &overrides, &mut homes, executor);
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

/// A newly installed CLI may not create its profile directory until login.
/// Run these probes through setup's bounded, cancellable executor.
fn discover_installed_harnesses(
    user_home: Option<&Path>,
    overrides: &BTreeMap<HarnessKind, PathBuf>,
    homes: &mut Vec<DiscoveredHome>,
    executor: &impl CommandExecutor,
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
        let probe = CommandSpec::new(kind.cli_binary_name(), ["--version"])
            .purpose("detect installed harness before first login");
        match executor.execute(&probe) {
            Ok(output) if output.status == 0 => homes.push(DiscoveredHome {
                kind,
                path: home,
                authenticated: false,
            }),
            Ok(_) => {}
            Err(error) => tracing::debug!(
                harness = kind.id(),
                "installation probe unavailable: {error:#}"
            ),
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
        .map(|(kind, path, is_default_home)| DiscoveredHome {
            authenticated: harness_is_authenticated_with(
                &probe_profile(kind, &path),
                is_default_home,
                executor,
            ),
            kind,
            path,
        })
        .collect()
}

/// A profile standing in for a home discovery found but the user has not
/// configured. It carries no environment, which is all the authentication gate
/// needs: how a profile authenticates is decided by its home, not its key.
fn probe_profile(kind: HarnessKind, home: &Path) -> HarnessProfile {
    HarnessProfile {
        enabled: true,
        kind,
        home: home.to_path_buf(),
        environment: Default::default(),
        context_window_bytes: None,
        subagents: Default::default(),
        guardian_review_model: None,
    }
}

pub(crate) fn harness_is_authenticated_with_executor(
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> bool {
    let is_default_home = dirs::home_dir()
        .is_some_and(|user_home| profile.home == user_home.join(profile.kind.default_home_leaf()));
    harness_is_authenticated_with(profile, is_default_home, executor)
}

/// Whether this profile can talk to its service without a login first.
///
/// An API-key profile is proven by its harness configuration file, because its
/// key lives in the profile's `environment` rather than in a credential file;
/// [`HarnessProfile::authentication_marker`] already names the right file for
/// either case.
fn harness_is_authenticated_with(
    profile: &HarnessProfile,
    is_default_home: bool,
    executor: &impl CommandExecutor,
) -> bool {
    let kind = profile.kind;
    let home = profile.home.as_path();
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
    let command = CommandSpec::new(
        "security",
        [
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ],
    )
    .purpose("check Claude Code authentication in the macOS Keychain");
    let Ok(output) = executor.execute(&command) else {
        return false;
    };
    output.status == 0 && claude_credentials_contain_login(&output.stdout)
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
    let origin = origin.trim();
    let path = origin
        .strip_prefix("https://github.com/")
        .or_else(|| origin.strip_prefix("http://github.com/"))
        .or_else(|| origin.strip_prefix("git@github.com:"))
        .or_else(|| origin.strip_prefix("ssh://git@github.com/"))
        // Config accepts owner/repository shorthand, and import uses the same
        // parser to compare that configured source with `git remote` output.
        .unwrap_or(origin);
    let path = path.trim_end_matches(".git");
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    if owner.is_empty()
        || repository.is_empty()
        || parts.next().is_some()
        || owner.chars().any(char::is_whitespace)
        || repository.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(GithubRepository {
        owner: owner.to_owned(),
        repository: repository.to_owned(),
    })
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
        let id = unique_id(&config.profiles, home.kind.id());
        config.profiles.insert(
            id,
            HarnessProfile {
                enabled: true,
                kind: home.kind,
                home: home.path.clone(),
                environment: Default::default(),
                context_window_bytes: None,
                subagents: Default::default(),
                guardian_review_model: None,
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

//! Plain-stdio first-run configuration for Hel.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};

use crate::doctor::{
    CheckStatus, DoctorCheck, DoctorOptions, all_ready, apple_container_daemon_check,
    current_apple_platform, local_docker_runtime_check, local_podman_runtime_check, probe_executor,
    render_human, run_with_config_path,
};
use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandSpec,
    ContainerTemplate as RuntimeContainerTemplate, ProcessExecutor,
    TargetTemplate as RuntimeTargetTemplate, run_setup_smoke_test,
};
use mj_core::config::{
    AwsAddressSource, Config, ContainerTemplate, HarnessHost, HarnessKind, HarnessProfile,
    PermissionMode, ProjectBundle, ProjectRepository, SshConnection, TargetTemplate,
    unique_config_id as unique_id, validate_id,
};

/// AWS credential detection must never stall an interactive first run, so the
/// probe commands share a bounded deadline.
const AWS_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// The user every Hel launch template image boots with; see
/// scripts/update-runson-launch-template.sh.
const DEFAULT_AWS_SSH_USER: &str = "ubuntu";
const AWS_TARGET_ID: &str = "aws";

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
    fn id(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
        }
    }

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
    pub usable: bool,
    pub detail: String,
    /// The fix `mj doctor` would print for this runtime, carried through so
    /// setup never invents its own remediation wording.
    pub remediation: Option<String>,
}

/// An AWS identity that `aws sts get-caller-identity` confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAccount {
    pub account: String,
    pub arn: String,
    /// The CLI's configured default region, when it has one.
    pub region: Option<String>,
}

/// The answers that become a `[targets.aws]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsTargetInput {
    pub launch_template: String,
    pub region: String,
    pub ssh_user: String,
    pub identity_file: Option<PathBuf>,
}

/// Which kind of SSH target the user chose in the SSH step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshTargetKind {
    Bare { permissions: PermissionMode },
    Podman { image: String },
    Docker { image: String },
}

/// The answers that become a `[targets.<name>]` SSH entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTargetInput {
    pub name: String,
    pub host: String,
    pub kind: SshTargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupDiscovery {
    pub homes: Vec<DiscoveredHome>,
    pub repository: Option<GithubRepository>,
    pub runtimes: Vec<RuntimeProbe>,
    /// `None` when this host has no working AWS CLI credentials, in which case
    /// setup never offers an AWS target.
    pub aws: Option<AwsAccount>,
    /// Concrete `Host` aliases read from `~/.ssh/config`; empty when the file
    /// is absent or only defines wildcard blocks.
    pub ssh_hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOutcome {
    Written,
    Cancelled,
}

/// Give an unconfigured terminal installation a local Codex profile and target
/// without making remote/container setup a prerequisite for explicit session
/// creation. This only writes configuration; it never creates a session.
pub fn initialize_local_startup_config(config_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let config = Config::load_from(config_path)?;
        if config.is_unconfigured() {
            let kind = HarnessKind::Codex;
            let home = std::env::var_os(kind.home_env())
                .map(|value| kind.home_from_environment(value))
                .or_else(|| dirs::home_dir().map(|home| home.join(kind.default_home_leaf())))
                .context("locate Codex home for the default local profile")?;
            let home = std::path::absolute(home).context("resolve Codex profile home")?;
            Config::update_to(config_path, |fresh| {
                if fresh.is_unconfigured() {
                    configure_local_startup(fresh, home);
                }
                Ok(())
            })?;
        }
    }
    // Local bare targets are unsupported on Windows; retain explicit setup.
    #[cfg(not(unix))]
    let _ = config_path;
    Ok(())
}

#[cfg(unix)]
fn configure_local_startup(config: &mut Config, codex_home: PathBuf) {
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: codex_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
}

/// Run the setup dialog using the user's normal standard input and output.
pub fn run_setup_dialog(config_path: &Path) -> Result<SetupOutcome> {
    // Prerequisite probes run under doctor's per-probe deadline, so a wedged
    // container socket cannot stall the first run; only the smoke test, which
    // may pull an image, is allowed to take as long as it needs.
    let probes = probe_executor();
    let discovery = discover_current(&probes);
    let stdout = io::stdout();
    let mut input = ReadlinePrompter::default();
    run_setup_dialog_inner(
        &mut input,
        &mut stdout.lock(),
        config_path,
        &discovery,
        &ProcessExecutor,
        &probes,
    )
}

pub fn discover_current(executor: &impl CommandExecutor) -> SetupDiscovery {
    let home = dirs::home_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    SetupDiscovery {
        homes: discover_profiles(executor),
        repository: discover_github_repository(executor, &cwd),
        runtimes: discover_runtimes(executor),
        aws: detect_aws(&CancellableProcessExecutor::with_timeout(AWS_PROBE_TIMEOUT)),
        ssh_hosts: discover_ssh_hosts(home.as_deref()),
    }
}

/// The installed agent homes alone. Callers that only add profiles use this
/// instead of `discover_current`, which also runs container, AWS, and SSH
/// probes whose results they would discard.
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
    probe_local_runtimes(executor, cfg!(target_os = "macos"))
}

/// A configuration holding the discovered profiles and nothing else, for
/// merging into an existing configuration.
pub fn profiles_config(homes: &[DiscoveredHome]) -> Config {
    build_config_with_runtimes(homes, None, &[], None, None)
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
        environment: BTreeMap::new(),
        context_window_bytes: None,
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
pub fn probe_local_runtimes(executor: &impl CommandExecutor, is_macos: bool) -> Vec<RuntimeProbe> {
    let mut probes = vec![
        runtime_probe_from_check(RuntimeKind::Podman, local_podman_runtime_check(executor)),
        runtime_probe_from_check(RuntimeKind::Docker, local_docker_runtime_check(executor)),
    ];
    if is_macos {
        probes.push(runtime_probe_from_check(
            RuntimeKind::AppleContainer,
            apple_container_daemon_check(executor),
        ));
    }
    probes
}

fn runtime_probe_from_check(kind: RuntimeKind, check: crate::doctor::DoctorCheck) -> RuntimeProbe {
    RuntimeProbe {
        kind,
        usable: check.status == CheckStatus::Ready,
        detail: check.detail,
        remediation: check.remediation,
    }
}

/// Detect a usable AWS CLI identity on this host.
///
/// Returns `None` whenever the CLI is missing or its credentials do not work,
/// so setup can skip the AWS step instead of prompting for a target that could
/// never launch.
pub fn detect_aws(executor: &impl CommandExecutor) -> Option<AwsAccount> {
    let identity = CommandSpec::new("aws", ["sts", "get-caller-identity", "--output", "json"])
        .purpose("detect AWS credentials");
    let output = executor.execute(&identity).ok()?;
    if output.status != 0 {
        return None;
    }
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let account = identity.get("Account")?.as_str()?.to_owned();
    let arn = identity.get("Arn")?.as_str()?.to_owned();
    Some(AwsAccount {
        account,
        arn,
        region: configured_aws_region(executor),
    })
}

fn configured_aws_region(executor: &impl CommandExecutor) -> Option<String> {
    let command = CommandSpec::new("aws", ["configure", "get", "region"])
        .purpose("read the default AWS region");
    let output = executor.execute(&command).ok()?;
    if output.status != 0 {
        return None;
    }
    let region = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!region.is_empty()).then_some(region)
}

pub fn build_config(
    homes: &[DiscoveredHome],
    repository: Option<&GithubRepository>,
    runtime: RuntimeKind,
    image: &str,
) -> Config {
    build_config_with_runtime(homes, repository, Some((runtime, image)), None, None)
}

fn build_config_with_runtime(
    homes: &[DiscoveredHome],
    repository: Option<&GithubRepository>,
    runtime: Option<(RuntimeKind, &str)>,
    aws: Option<&AwsTargetInput>,
    ssh: Option<&SshTargetInput>,
) -> Config {
    build_config_with_runtimes(
        homes,
        repository,
        &runtime.into_iter().collect::<Vec<_>>(),
        aws,
        ssh,
    )
}

fn build_config_with_runtimes(
    homes: &[DiscoveredHome],
    repository: Option<&GithubRepository>,
    runtimes: &[(RuntimeKind, &str)],
    aws: Option<&AwsTargetInput>,
    ssh: Option<&SshTargetInput>,
) -> Config {
    let mut config = Config::default();
    for home in homes {
        let id = unique_id(&config.profiles, home.kind.id());
        config.profiles.insert(
            id,
            HarnessProfile {
                enabled: true,
                kind: home.kind,
                home: home.path.clone(),
                environment: BTreeMap::new(),
                context_window_bytes: None,
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
    for (runtime, image) in runtimes {
        let (target_id, target) = local_runtime_target(*runtime, image);
        config.targets.insert(target_id.to_owned(), target);
    }
    if let Some(aws) = aws {
        config.targets.insert(
            AWS_TARGET_ID.to_owned(),
            TargetTemplate::AwsEc2 {
                aws_profile: None,
                region: aws.region.clone(),
                launch_template: aws.launch_template.clone(),
                launch_template_version: None,
                ssh_user: aws.ssh_user.clone(),
                address_source: AwsAddressSource::default(),
                identity_file: aws.identity_file.clone(),
                ssh_args: vec![],
            },
        );
    }
    if let Some(ssh) = ssh {
        // Leave user and identity file unset: the SSH config alias already
        // carries whatever the user configured for this host.
        let connection = SshConnection {
            host: ssh.host.clone(),
            user: None,
            identity_file: None,
            extra_args: vec![],
        };
        let target = match &ssh.kind {
            SshTargetKind::Bare { permissions } => TargetTemplate::SshBare {
                ssh: connection,
                permissions: *permissions,
                workspace_prefix: default_ssh_workspace_prefix(),
            },
            SshTargetKind::Podman { image } => TargetTemplate::SshPodman {
                ssh: connection,
                container: ContainerTemplate {
                    build_cache: None,
                    image: image.clone(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
            SshTargetKind::Docker { image } => TargetTemplate::SshDocker {
                ssh: connection,
                container: ContainerTemplate {
                    build_cache: None,
                    image: image.clone(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        };
        // The dialog already refuses a name that collides, so this only guards
        // a caller that builds a config without asking: a chosen SSH name must
        // never silently replace a target configured moments earlier.
        config
            .targets
            .insert(unique_id(&config.targets, &ssh.name), target);
    }
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
        environment: BTreeMap::new(),
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

/// A new machine's directory. The file names it, so a machine written here
/// never falls back to the former directory a hand-written one keeps.
fn default_ssh_workspace_prefix() -> PathBuf {
    PathBuf::from(mj_core::config::DEFAULT_WORKSPACE_PREFIX)
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

/// Ask the setup questions, write the configuration, and report on it.
///
/// The smoke test and the closing doctor report run through different
/// executors on purpose: a smoke test may pull a multi-gigabyte image and must
/// not be given a deadline, while every prerequisite probe must answer quickly
/// or be reported as a fixable check.
pub fn run_setup_dialog_with(
    input: &mut impl BufRead,
    output: &mut impl Write,
    config_path: &Path,
    discovery: &SetupDiscovery,
    smoke_executor: &impl CommandExecutor,
    probe_executor: &impl CommandExecutor,
) -> Result<SetupOutcome> {
    run_setup_dialog_inner(
        input,
        output,
        config_path,
        discovery,
        smoke_executor,
        probe_executor,
    )
}

fn run_setup_dialog_inner(
    input: &mut impl SetupPrompter,
    output: &mut impl Write,
    config_path: &Path,
    discovery: &SetupDiscovery,
    smoke_executor: &impl CommandExecutor,
    probe_executor: &impl CommandExecutor,
) -> Result<SetupOutcome> {
    let existing = Config::load_from(config_path)?;
    writeln!(output, "Welcome to Mjolnir setup.")?;
    writeln!(output)?;
    write_discovered_homes(output, &discovery.homes)?;
    write_repository(output, discovery.repository.as_ref())?;
    write_runtimes(output, &discovery.runtimes)?;

    let runtimes = if discovery.runtimes.iter().any(|runtime| runtime.usable) {
        let image = prompt(
            input,
            output,
            &format!("Container image [{DEFAULT_IMAGE}]: "),
        )?;
        let image = if image.is_empty() {
            DEFAULT_IMAGE.to_owned()
        } else {
            image
        };
        discovery
            .runtimes
            .iter()
            .filter(|runtime| runtime.usable)
            .map(|runtime| (runtime.kind, image.clone()))
            .collect::<Vec<_>>()
    } else {
        writeln!(
            output,
            "No usable container runtime found; raw localhost will still be configured."
        )?;
        Vec::new()
    };
    let aws = prompt_aws_target(input, output, discovery.aws.as_ref())?;
    let runtime_choices = runtimes
        .iter()
        .map(|(runtime, image)| (*runtime, image.as_str()))
        .collect::<Vec<_>>();
    // Build what the earlier answers already claimed, so the SSH step can
    // refuse a target name that would replace one of them.
    let configured = build_config_with_runtimes(
        &discovery.homes,
        discovery.repository.as_ref(),
        &runtime_choices,
        aws.as_ref(),
        None,
    );
    let ssh = prompt_ssh_target(input, output, &discovery.ssh_hosts, &configured.targets)?;
    let config = build_config_with_runtimes(
        &discovery.homes,
        discovery.repository.as_ref(),
        &runtime_choices,
        aws.as_ref(),
        ssh.as_ref(),
    );
    config.validate()?;
    let additions = reconcile_setup(input, output, &existing, config)?;
    let runtimes = runtimes
        .into_iter()
        .filter(|(runtime, image)| {
            let (_, target) = local_runtime_target(*runtime, image);
            additions.targets.values().any(|added| added == &target)
        })
        .collect::<Vec<_>>();

    writeln!(output)?;
    write_summary(output, config_path, &additions, &runtimes)?;
    let confirmation = prompt(input, output, "Write this configuration? [y/N]: ")?;
    if !matches!(confirmation.to_ascii_lowercase().as_str(), "y" | "yes") {
        writeln!(output, "Setup cancelled.")?;
        return Ok(SetupOutcome::Cancelled);
    }

    writeln!(output, "Writing {}...", config_path.display())?;
    Config::update_to(config_path, |latest| {
        apply_setup_additions(latest, &additions)
    })?;
    // A failed smoke test is a fixable prerequisite, not a reason to abandon
    // the run: the configuration is already written, and this is exactly when
    // the closing report's remediations matter most.
    let smoke_failures = runtimes
        .iter()
        .filter_map(|(runtime, image)| {
            let target = smoke_target(*runtime, image);
            run_smoke_test(output, &target, smoke_executor)
                .err()
                .map(|error| smoke_failure_check(*runtime, image, &error))
        })
        .collect();
    write_doctor_report(output, config_path, probe_executor, smoke_failures)?;
    writeln!(
        output,
        "Advanced users can edit TOML for extra profiles, virtual monorepos, SSH, and AWS."
    )?;
    writeln!(
        output,
        "Run `mj` to open Mjolnir, then press n in the Sessions pane to start your first session."
    )?;
    Ok(SetupOutcome::Written)
}

/// Setup only adds entries. Existing identifiers may belong to live sessions.
fn reconcile_setup(
    input: &mut impl SetupPrompter,
    output: &mut impl Write,
    existing: &Config,
    discovered: Config,
) -> Result<Config> {
    let mut additions = existing.setup_additions(&discovered);
    for id in additions.profiles.keys() {
        writeln!(output, "Adding discovered profile {id}.")?;
    }
    for id in additions.bundles.keys() {
        writeln!(output, "Adding repository bundle {id}.")?;
    }
    for (id, target) in &discovered.targets {
        if !existing.targets.contains_key(id) {
            continue;
        }
        let alternate = additions
            .targets
            .iter()
            .find(|(_, added)| *added == target)
            .map(|(id, _)| id.clone());
        let Some(alternate) = alternate else { continue };
        writeln!(
            output,
            "Target {id} already has different settings. Existing sessions will keep using it."
        )?;
        let answer = prompt(
            input,
            output,
            &format!("Keep {id}, or add the discovered settings as {alternate}? [K/a]: "),
        )?;
        if !matches!(answer.to_ascii_lowercase().as_str(), "a" | "add") {
            additions.targets.remove(&alternate);
            writeln!(
                output,
                "Keeping target {id}; discovered settings were not added."
            )?;
        }
    }
    Ok(additions)
}

fn apply_setup_additions(latest: &mut Config, additions: &Config) -> Result<()> {
    fn add<T: Clone>(
        section: &str,
        latest: &mut BTreeMap<String, T>,
        additions: &BTreeMap<String, T>,
        same: impl Fn(&T, &T) -> bool,
    ) -> Result<()> {
        for (id, value) in additions {
            if latest.values().any(|existing| same(existing, value)) {
                continue;
            }
            ensure!(
                !latest.contains_key(id),
                "{section} {id:?} changed while setup was open; no configuration was written. Rerun mj setup to review the current settings"
            );
            latest.insert(id.clone(), value.clone());
        }
        Ok(())
    }
    add(
        "profile",
        &mut latest.profiles,
        &additions.profiles,
        HarnessProfile::same_installation,
    )?;
    add(
        "bundle",
        &mut latest.bundles,
        &additions.bundles,
        PartialEq::eq,
    )?;
    add(
        "target",
        &mut latest.targets,
        &additions.targets,
        PartialEq::eq,
    )?;
    latest.validate()
}

fn write_discovered_homes(output: &mut impl Write, homes: &[DiscoveredHome]) -> Result<()> {
    writeln!(output, "Harness homes:")?;
    if homes.is_empty() {
        writeln!(
            output,
            "  No existing {} homes found.",
            HarnessKind::every_display_name_or()
        )?;
    }
    for home in homes {
        let authentication = if home.authenticated {
            "authenticated"
        } else {
            "not authenticated"
        };
        writeln!(
            output,
            "  {}: {} ({authentication}){}",
            home.kind.display_name(),
            home.path.display(),
            match home.kind.unsandboxed_guardian_warning() {
                Some(warning) => format!(" — {warning}"),
                None => String::new(),
            }
        )?;
    }
    Ok(())
}

fn write_repository(output: &mut impl Write, repository: Option<&GithubRepository>) -> Result<()> {
    match repository {
        Some(repository) => writeln!(
            output,
            "GitHub origin: {} (a one-repository bundle will be created)",
            repository.source()
        )?,
        None => writeln!(
            output,
            "GitHub origin: none detected in the current directory."
        )?,
    }
    Ok(())
}

fn write_runtimes(output: &mut impl Write, runtimes: &[RuntimeProbe]) -> Result<()> {
    writeln!(output, "Local runtimes:")?;
    for runtime in runtimes {
        let state = if runtime.usable {
            "usable"
        } else {
            "unavailable"
        };
        if runtime.detail.is_empty() {
            writeln!(output, "  {}: {state}", runtime.kind.label())?;
        } else {
            writeln!(
                output,
                "  {}: {state} ({})",
                runtime.kind.label(),
                runtime.detail
            )?;
        }
        if let Some(remediation) = &runtime.remediation {
            writeln!(output, "    remediation: {remediation}")?;
        }
    }
    Ok(())
}

/// Offer an AWS EC2 target, but only when this host already has working AWS
/// credentials. Without them the step prints one line and asks nothing.
fn prompt_aws_target(
    input: &mut impl SetupPrompter,
    output: &mut impl Write,
    account: Option<&AwsAccount>,
) -> Result<Option<AwsTargetInput>> {
    let Some(account) = account else {
        writeln!(
            output,
            "AWS: no working `aws` CLI credentials found; skipping the AWS target."
        )?;
        return Ok(None);
    };
    writeln!(
        output,
        "AWS: credentials are valid for account {} ({}).",
        account.account, account.arn
    )?;
    let answer = prompt(input, output, "Add an AWS EC2 target? [y/N]: ")?;
    if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(None);
    }

    let launch_template = prompt(input, output, "Launch template name: ")?;
    if launch_template.is_empty() {
        writeln!(
            output,
            "A launch template name is required; skipping the AWS target."
        )?;
        return Ok(None);
    }

    let region_label = match &account.region {
        Some(region) => format!("Region [{region}]: "),
        None => "Region: ".to_owned(),
    };
    let region = prompt(input, output, &region_label)?;
    let region = if region.is_empty() {
        match &account.region {
            Some(region) => region.clone(),
            None => {
                writeln!(output, "A region is required; skipping the AWS target.")?;
                return Ok(None);
            }
        }
    } else {
        region
    };

    let ssh_user = prompt(
        input,
        output,
        &format!("SSH user [{DEFAULT_AWS_SSH_USER}]: "),
    )?;
    let ssh_user = if ssh_user.is_empty() {
        DEFAULT_AWS_SSH_USER.to_owned()
    } else {
        ssh_user
    };
    let identity_file = prompt(input, output, "SSH identity file (optional): ")?;

    Ok(Some(AwsTargetInput {
        launch_template,
        region,
        ssh_user,
        identity_file: (!identity_file.is_empty()).then(|| PathBuf::from(identity_file)),
    }))
}

/// Offer an SSH target built from the aliases in `~/.ssh/config`.
///
/// With no aliases the step prints one line and asks nothing, the same way the
/// AWS step reports skipping.
fn prompt_ssh_target(
    input: &mut impl SetupPrompter,
    output: &mut impl Write,
    aliases: &[String],
    configured: &BTreeMap<String, TargetTemplate>,
) -> Result<Option<SshTargetInput>> {
    if aliases.is_empty() {
        writeln!(
            output,
            "SSH: no host aliases found in ~/.ssh/config; skipping the SSH target."
        )?;
        return Ok(None);
    }
    writeln!(output, "SSH: hosts found in ~/.ssh/config:")?;
    for (index, alias) in aliases.iter().enumerate() {
        writeln!(output, "  {}) {alias}", index + 1)?;
    }
    let answer = prompt(input, output, "Add an SSH target? [y/N]: ")?;
    if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(None);
    }

    let choice = prompt(
        input,
        output,
        &format!("Host number 1-{} or a host name: ", aliases.len()),
    )?;
    let host = match choice.parse::<usize>() {
        Ok(index) if (1..=aliases.len()).contains(&index) => aliases[index - 1].clone(),
        _ if !choice.is_empty() => choice,
        _ => {
            writeln!(output, "A host is required; skipping the SSH target.")?;
            return Ok(None);
        }
    };

    let kind = loop {
        let runtime = prompt(
            input,
            output,
            "Container runtime on that host, podman, docker, or bare [podman]: ",
        )?;
        let runtime = runtime.to_ascii_lowercase();
        if matches!(runtime.as_str(), "n" | "no" | "bare") {
            let permissions = loop {
                let mode = prompt(
                    input,
                    output,
                    "Raw-host permissions, guardian or yolo [guardian]: ",
                )?;
                match mode.to_ascii_lowercase().as_str() {
                    "" | "guardian" => break PermissionMode::Guardian,
                    "yolo" => break PermissionMode::Yolo,
                    _ => writeln!(output, "Permissions must be `guardian` or `yolo`.")?,
                }
            };
            break SshTargetKind::Bare { permissions };
        }
        let kind = if matches!(runtime.as_str(), "" | "y" | "yes" | "podman") {
            SshTargetKind::Podman {
                image: String::new(),
            }
        } else if runtime == "docker" {
            SshTargetKind::Docker {
                image: String::new(),
            }
        } else {
            writeln!(
                output,
                "Runtime must be `podman`, `docker`, or `bare`; please choose again."
            )?;
            continue;
        };
        let image = prompt(
            input,
            output,
            &format!("Container image [{DEFAULT_IMAGE}]: "),
        )?;
        let image = if image.is_empty() {
            DEFAULT_IMAGE.to_owned()
        } else {
            image
        };
        break match kind {
            SshTargetKind::Podman { .. } => SshTargetKind::Podman { image },
            SshTargetKind::Docker { .. } => SshTargetKind::Docker { image },
            SshTargetKind::Bare { .. } => unreachable!("bare runtime returned above"),
        };
    };

    let Some(name) = prompt_ssh_target_name(input, output, &host, configured)? else {
        return Ok(None);
    };

    Ok(Some(SshTargetInput { name, host, kind }))
}

/// Ask for the SSH target's name until the answer is a usable target id.
///
/// Both failures are caught here rather than by `config.validate()` after every
/// question has been asked: an invalid id would otherwise discard the whole
/// dialog, and a name that is already taken would silently replace the target
/// it collides with.
fn prompt_ssh_target_name(
    input: &mut impl SetupPrompter,
    output: &mut impl Write,
    host: &str,
    configured: &BTreeMap<String, TargetTemplate>,
) -> Result<Option<String>> {
    loop {
        let Some(answer) = prompt_line(input, output, &format!("Target name [{host}]: "))? else {
            writeln!(output, "Input ended; skipping the SSH target.")?;
            return Ok(None);
        };
        let name = if answer.is_empty() {
            host.to_owned()
        } else {
            answer
        };
        if let Err(error) = validate_id("target", &name) {
            writeln!(output, "{error}")?;
            continue;
        }
        if configured.contains_key(&name) {
            writeln!(
                output,
                "Target {name} is already configured; choose another name."
            )?;
            continue;
        }
        return Ok(Some(name));
    }
}

/// Phrase a failed setup smoke test the way `mj doctor --smoke` phrases the
/// same failure, so it joins the closing report instead of ending the run.
fn smoke_failure_check(runtime: RuntimeKind, image: &str, error: &anyhow::Error) -> DoctorCheck {
    let scope = match runtime {
        RuntimeKind::Docker => "Disposable run/exec/remove and OverlayFS attachment smoke test",
        RuntimeKind::Podman | RuntimeKind::AppleContainer => {
            "Disposable run/exec/remove smoke test"
        }
    };
    DoctorCheck::fixable(
        format!("runtime.{}.smoke", runtime.id()),
        format!("{} smoke test", runtime.label()),
        format!("{scope} failed for image {image}: {error:#}"),
        format!(
            "Fix the configured image or the {} runtime, then run `mj doctor --smoke` again.",
            runtime.label()
        ),
    )
}

/// End setup with the same report `mj doctor` prints, so the user gets one
/// ready/fixable summary with remediations instead of two different signals.
///
/// `extra` carries anything setup itself learned that doctor cannot repeat
/// without the opt-in smoke test.
fn write_doctor_report(
    output: &mut impl Write,
    config_path: &Path,
    executor: &impl CommandExecutor,
    extra: Vec<DoctorCheck>,
) -> Result<()> {
    writeln!(output)?;
    writeln!(output, "Running `mj doctor` checks on the new config...")?;
    let mut checks = run_with_config_path(
        config_path,
        executor,
        current_apple_platform(executor),
        DoctorOptions { smoke: false },
    );
    checks.extend(extra);
    render_human(&checks, output)?;
    if all_ready(&checks) {
        writeln!(output, "Every check is ready.")?;
    } else {
        writeln!(
            output,
            "Apply the remediations above, then rerun `mj doctor`."
        )?;
    }
    Ok(())
}

fn prompt(input: &mut impl SetupPrompter, output: &mut impl Write, label: &str) -> Result<String> {
    Ok(prompt_line(input, output, label)?.unwrap_or_default())
}

/// Read one answer, reporting `None` once the input has ended.
///
/// Every question but one treats the end of input as an empty answer and takes
/// its default. A question that must be asked again until it is answered needs
/// the difference, or it would loop forever against a closed stdin.
fn prompt_line(
    input: &mut impl SetupPrompter,
    output: &mut impl Write,
    label: &str,
) -> Result<Option<String>> {
    input.read_prompt(output, label)
}

trait SetupPrompter {
    fn read_prompt(&mut self, output: &mut dyn Write, label: &str) -> Result<Option<String>>;
}

impl<R: BufRead> SetupPrompter for R {
    fn read_prompt(&mut self, output: &mut dyn Write, label: &str) -> Result<Option<String>> {
        write!(output, "{label}")?;
        output.flush()?;
        let mut answer = String::new();
        let read = self.read_line(&mut answer).context("read setup response")?;
        Ok((read > 0).then(|| answer.trim().to_owned()))
    }
}

#[derive(Default)]
struct ReadlinePrompter(crate::readline::LineReader);

impl SetupPrompter for ReadlinePrompter {
    fn read_prompt(&mut self, output: &mut dyn Write, label: &str) -> Result<Option<String>> {
        output.flush()?;
        self.0.read_line(label).context("read setup response")
    }
}

fn write_summary(
    output: &mut impl Write,
    config_path: &Path,
    config: &Config,
    runtimes: &[(RuntimeKind, String)],
) -> Result<()> {
    writeln!(output, "Mjolnir will add to {}:", config_path.display())?;
    let counted = mj_core::text::counted;
    writeln!(
        output,
        "  {}",
        counted(config.profiles.len(), "profile", "profiles")
    )?;
    writeln!(
        output,
        "  {}",
        counted(config.bundles.len(), "bundle", "bundles")
    )?;
    if config
        .targets
        .values()
        .any(|target| matches!(target, TargetTemplate::LocalBare))
    {
        writeln!(
            output,
            "  raw localhost target using configured harness homes directly"
        )?;
    }
    for (runtime, image) in runtimes {
        writeln!(output, "  {} target using {image}", runtime.label())?;
    }
    for id in config.targets.keys() {
        writeln!(output, "  target id: {id}")?;
    }
    if let Some(TargetTemplate::AwsEc2 {
        launch_template,
        region,
        ..
    }) = config.targets.get(AWS_TARGET_ID)
    {
        writeln!(
            output,
            "  AWS EC2 target using launch template {launch_template} in {region}"
        )?;
    }
    for (id, target) in &config.targets {
        match target {
            TargetTemplate::SshBare { ssh, .. } => {
                writeln!(output, "  SSH target {id} on {} (no container)", ssh.host)?;
            }
            TargetTemplate::SshPodman { ssh, container, .. } => {
                writeln!(
                    output,
                    "  SSH target {id} on {} using Podman image {}",
                    ssh.host, container.image
                )?;
            }
            TargetTemplate::SshDocker { ssh, container } => {
                writeln!(
                    output,
                    "  SSH target {id} on {} using Docker image {}",
                    ssh.host, container.image
                )?;
            }
            _ => {}
        }
    }
    if config_path.exists() {
        writeln!(
            output,
            "  Existing profiles, bundles, targets, and preferences will be preserved."
        )?;
    }
    Ok(())
}

fn smoke_target(runtime: RuntimeKind, image: &str) -> RuntimeTargetTemplate {
    let container = RuntimeContainerTemplate {
        build_cache: None,
        image: image.to_owned(),
        pull_policy: Default::default(),
        extra_run_args: vec![],
        workspace_storage: Default::default(),
    };
    match runtime {
        RuntimeKind::Podman => RuntimeTargetTemplate::LocalPodman(container),
        RuntimeKind::Docker => RuntimeTargetTemplate::LocalDocker(container),
        RuntimeKind::AppleContainer => RuntimeTargetTemplate::AppleContainer(container),
    }
}

fn run_smoke_test(
    output: &mut impl Write,
    target: &RuntimeTargetTemplate,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let smoke_id = format!(
        "setup-{}-{:x}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let description = match target {
        RuntimeTargetTemplate::LocalDocker(_) => {
            "Smoke test: verifying a disposable container and writable OverlayFS attachment..."
        }
        _ => "Smoke test: verifying a disposable container...",
    };
    writeln!(output, "{description}")?;
    if let Some(announcement) = smoke_download_announcement(target) {
        writeln!(output, "{announcement}")?;
    }
    // Nothing is printed while the test runs, so close the wait with its
    // outcome and how long it took (launch finding R3-10).
    let started = std::time::Instant::now();
    let result = run_setup_smoke_test(target, &smoke_id, executor);
    let took = mj_core::activity::describe_duration(
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    match &result {
        Ok(()) => writeln!(output, "Smoke test passed in {took}.")?,
        Err(_) => writeln!(
            output,
            "Smoke test failed after {took}; the checks below say what to fix."
        )?,
    }
    result
}

/// Download size of [`mj_core::config::DEFAULT_CONTAINER_IMAGE`], as the
/// launch campaign measured it (1.93 GB on 2026-09-23). Update it when the
/// image grows or shrinks noticeably.
const DEFAULT_IMAGE_DOWNLOAD_SIZE: &str = "about 2 GB";

/// The engine pulls a missing image as part of the smoke test's first
/// command and prints nothing while it does, so say what may happen before
/// the wait starts.
fn smoke_download_announcement(target: &RuntimeTargetTemplate) -> Option<String> {
    let (engine, container) = match target {
        RuntimeTargetTemplate::LocalPodman(container) => ("Podman", container),
        RuntimeTargetTemplate::LocalDocker(container) => ("Docker", container),
        RuntimeTargetTemplate::AppleContainer(container) => ("Apple container", container),
        _ => return None,
    };
    let image = &container.image;
    let size = if image == mj_core::config::DEFAULT_CONTAINER_IMAGE {
        format!(" ({DEFAULT_IMAGE_DOWNLOAD_SIZE})")
    } else {
        String::new()
    };
    Some(format!(
        "If {image} is not on this machine yet, {engine} downloads it first{size}. That can take several minutes, and nothing more is printed until it finishes."
    ))
}

#[cfg(test)]
mod tests;

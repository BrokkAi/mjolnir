//! Hel's versioned user configuration and domain model.
//!
//! This is intentionally a clean namespace. Nothing in this module reads or
//! migrates the legacy `mj` configuration tree.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable context identity for a bare project at the serialized path boundary.
pub fn raw_project_context_id(project_directory: &str) -> String {
    let digest = Sha256::digest(project_directory.trim().as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("remote-project-{suffix}")
}

fn default_phone_bind() -> String {
    "127.0.0.1:3765".to_owned()
}

const fn default_true() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default = "default_phone_bind")]
    pub bind: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub tailscale_detect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<PathBuf>,
}

impl Default for PhoneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: default_phone_bind(),
            tailscale_detect: true,
            tls_cert: None,
            tls_key: None,
        }
    }
}

impl PhoneConfig {
    fn validate(&self) -> Result<()> {
        let bind: std::net::SocketAddr = self
            .bind
            .parse()
            .with_context(|| format!("parse phone bind address {:?}", self.bind))?;
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            bail!("phone TLS requires both `tls_cert` and `tls_key`");
        }
        if !bind.ip().is_loopback() && self.tls_cert.is_none() {
            bail!("a non-loopback phone bind requires TLS");
        }
        Ok(())
    }
}

/// Automatic cross-harness review of every completed coding turn.
///
/// Review is armed here, in the one file that belongs to the machine rather
/// than to any surface: a session driven from a phone is reviewed on the same
/// terms as one driven from the terminal, and the person who set it can see
/// what they set. `profile` names a harness profile defined in this same file
/// -- the reviewer runs under that profile, and it must not be the profile the
/// session under review is using, or the "second opinion" is the same opinion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// Whether every completed turn is reviewed automatically. A one-off
    /// `/review` works whether or not this is set, as long as `profile` names
    /// a reviewer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_default_tier")]
    pub tier: crate::hel_review::lanes::ReviewTier,
    /// The harness profile the reviewing agents run under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Model and effort applied to every reviewing role, when the reviewing
    /// harness advertises such a selector. Absent means the profile's own
    /// default, which is what most configurations want.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_tier(tier: &crate::hel_review::lanes::ReviewTier) -> bool {
    *tier == crate::hel_review::lanes::ReviewTier::default()
}

impl ReviewConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Rejects a configuration that cannot review.
    ///
    /// Arming review without naming a reviewer is a configuration mistake with
    /// no sensible default -- Mjolnir will not pick a profile on the user's behalf,
    /// because which agent reviews is the most consequential review setting.
    /// Naming a disabled profile is invalid because neither automatic nor
    /// one-off reviews may start new work with it.
    fn validate(&self, profiles: &BTreeMap<String, HarnessProfile>) -> Result<()> {
        if let Some(profile_id) = self.profile.as_ref()
            && let Some(profile) = profiles.get(profile_id)
        {
            if !profile.enabled {
                bail!("[review] profile {profile_id:?} is disabled");
            }
            if !profile.kind.supports_injected_mcp() {
                bail!(
                    "Muse Code cannot be a reviewer because muse-acp does not accept the required MCP tools"
                );
            }
        }
        if self.enabled && self.profile.is_none() {
            bail!(
                "[review] enabled = true needs `profile` naming the harness profile that reviews"
            );
        }
        if let Some(profile) = &self.profile
            && !profiles.contains_key(profile)
        {
            bail!("[review] profile {profile:?} is not a profile defined in this config");
        }
        Ok(())
    }

    /// Whether a turn review can run at all: it needs a reviewer, armed or not.
    #[must_use]
    pub fn reviewer_profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }
}

pub const CONFIG_VERSION: u32 = 8;
pub const PRODUCT_DIR: &str = "mjolnir";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    Codex,
    Claude,
    Kimi,
    Grok,
    Deepseek,
    Muse,
}

/// The target-level execution policy Hel applies independently of the selected
/// harness. Raw targets may preserve configured approvals; isolated targets
/// force full access because their boundary contains the blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPolicy {
    /// Preserve the harness and profile's configured approval behavior.
    ConfiguredApprovals,
    /// Run every action without sandboxing or approval checks.
    Unconstrained,
}

/// Approval behavior selected for a named raw SSH target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Preserve the selected harness profile's approval behavior.
    Guardian,
    /// Run every action without sandboxing or approval checks.
    Yolo,
}

impl PermissionMode {
    pub const fn execution_policy(self) -> ExecutionPolicy {
        match self {
            Self::Guardian => ExecutionPolicy::ConfiguredApprovals,
            Self::Yolo => ExecutionPolicy::Unconstrained,
        }
    }
}

impl ExecutionPolicy {
    pub const fn is_unconstrained(self) -> bool {
        matches!(self, Self::Unconstrained)
    }
}

/// Harness-specific controls that collectively realize a target-level
/// execution policy. A harness may need more than one launch-time mechanism
/// in addition to an ACP mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionEnforcement {
    label: &'static str,
    acp_mode: Option<&'static str>,
    launch_flag: Option<&'static str>,
    launch_environment: Option<(&'static str, &'static str)>,
}

impl ExecutionEnforcement {
    /// Name reported to the UI for the mode this session runs in.
    pub const fn label(self) -> &'static str {
        self.label
    }

    /// The ACP mode to select after the session opens, when there is one.
    pub const fn acp_mode(self) -> Option<&'static str> {
        self.acp_mode
    }

    /// The launch flag to add to the bridge command line, when there is one.
    pub const fn launch_flag(self) -> Option<&'static str> {
        self.launch_flag
    }

    pub const fn launch_environment(self) -> Option<(&'static str, &'static str)> {
        self.launch_environment
    }
}

/// The file inside a harness home that proves the harness is logged in.
///
/// Setup, quota checks, credential sync, and the target-side worker all read
/// the same path, so it is decided here beside [`HarnessKind`] rather than in
/// any one of them.
pub fn harness_authentication_marker(kind: HarnessKind, home: &Path) -> PathBuf {
    home.join(match kind {
        HarnessKind::Codex => "auth.json",
        HarnessKind::Claude => ".credentials.json",
        HarnessKind::Kimi => "credentials/kimi-code.json",
        HarnessKind::Grok => "auth.json",
        HarnessKind::Deepseek => ".credentials.yaml",
        HarnessKind::Muse => "auth.json",
    })
}

impl HarnessKind {
    /// Translate a harness home into its process environment. Muse's config
    /// directory must be named `muse`, as required by the XDG directory layout.
    pub fn configure_home_environment(
        self,
        home: &Path,
        environment: &mut BTreeMap<String, String>,
    ) {
        let config_root = if self == Self::Muse {
            environment.insert(
                "XDG_DATA_HOME".into(),
                home.join(".data").to_string_lossy().into_owned(),
            );
            home.parent().unwrap_or(home)
        } else {
            home
        };
        environment.insert(
            self.home_env().into(),
            config_root.to_string_lossy().into_owned(),
        );
    }

    pub fn home_from_environment(self, value: impl AsRef<Path>) -> PathBuf {
        if self == Self::Muse {
            value.as_ref().join("muse")
        } else {
            value.as_ref().to_path_buf()
        }
    }

    pub const fn supports_injected_mcp(self) -> bool {
        !matches!(self, Self::Muse)
    }

    pub const ALL: [Self; 6] = [
        Self::Codex,
        Self::Claude,
        Self::Kimi,
        Self::Grok,
        Self::Deepseek,
        Self::Muse,
    ];

    /// Environment variable used to isolate this harness's configuration.
    pub const fn home_env(self) -> &'static str {
        match self {
            Self::Codex => "CODEX_HOME",
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Kimi => "KIMI_CODE_HOME",
            Self::Grok => "GROK_HOME",
            Self::Deepseek => "DSH_HOME",
            Self::Muse => "XDG_CONFIG_HOME",
        }
    }

    /// Directory beneath the user's home the harness uses when `home_env` is
    /// unset. The single source for both setup discovery and import.
    pub const fn default_home_leaf(self) -> &'static str {
        match self {
            Self::Codex => ".codex",
            Self::Claude => ".claude",
            Self::Kimi => ".kimi-code",
            Self::Grok => ".grok",
            Self::Deepseek => ".dsh",
            Self::Muse => ".config/muse",
        }
    }

    /// Lowercase stable identifier used in config, storage, and the HTTP API.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Deepseek => "deepseek",
            Self::Muse => "muse",
        }
    }

    /// Product name shown to people.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Kimi => "Kimi Code",
            Self::Grok => "Grok Build",
            Self::Deepseek => "DSH",
            Self::Muse => "Muse Code",
        }
    }

    /// How this harness realizes a target-level execution policy. Configured
    /// approvals preserve harness configuration; Codex selects guardian explicitly.
    pub const fn execution_enforcement(
        self,
        policy: ExecutionPolicy,
    ) -> Option<ExecutionEnforcement> {
        match (self, policy) {
            (Self::Muse, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "auto / sandbox-off",
                acp_mode: Some("auto"),
                launch_flag: None,
                launch_environment: Some(("MUSE_APPROVAL_MODE", "auto")),
            }),
            (Self::Codex, ExecutionPolicy::ConfiguredApprovals) => Some(ExecutionEnforcement {
                label: "agent / guardian",
                acp_mode: Some("agent"),
                launch_flag: None,
                launch_environment: Some(("INITIAL_AGENT_MODE", "agent")),
            }),
            (Self::Codex, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "agent-full-access",
                acp_mode: Some("agent-full-access"),
                launch_flag: None,
                launch_environment: Some(("INITIAL_AGENT_MODE", "agent-full-access")),
            }),
            (_, ExecutionPolicy::ConfiguredApprovals) => None,
            (Self::Claude, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "bypassPermissions / sandbox-off",
                acp_mode: Some("bypassPermissions"),
                launch_flag: None,
                launch_environment: None,
            }),
            (Self::Kimi, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "auto",
                acp_mode: Some("auto"),
                launch_flag: None,
                launch_environment: None,
            }),
            (Self::Grok, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "always-approve / sandbox-off",
                acp_mode: None,
                launch_flag: Some("--always-approve"),
                launch_environment: Some(("GROK_SANDBOX", "off")),
            }),
            (Self::Deepseek, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "danger-full-access",
                acp_mode: None,
                launch_flag: None,
                launch_environment: Some(("DSH_PERMISSION_MODE", "danger-full-access")),
            }),
        }
    }

    /// Apply the launch environment required to realize `policy`. The
    /// controller writes this into new launch configs, and the worker repeats
    /// it so persisted configs from older Hel versions acquire the same
    /// enforcement after an upgrade.
    pub fn configure_execution_environment(
        self,
        policy: ExecutionPolicy,
        environment: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        if self == Self::Muse && policy == ExecutionPolicy::Unconstrained {
            let args = environment.entry("MUSE_SERVE_ARGS".into()).or_default();
            if !args
                .split_whitespace()
                .any(|arg| arg == "--disable-sandbox")
            {
                args.push_str(" --disable-sandbox");
            }
        }
        if let Some((key, value)) = self
            .execution_enforcement(policy)
            .and_then(ExecutionEnforcement::launch_environment)
        {
            environment.insert(key.to_owned(), value.to_owned());
        }
        Ok(())
    }

    pub const fn supports_guardian_approvals(self) -> bool {
        matches!(self, Self::Codex | Self::Claude | Self::Grok | Self::Muse)
    }

    /// Shared warning for selecting a harness without guardian approvals on a
    /// raw target. Containers and remote instances run unconstrained by design
    /// and rely on target isolation instead.
    pub fn unsandboxed_guardian_warning(self) -> Option<String> {
        (!self.supports_guardian_approvals()).then(|| {
            format!(
                "DANGER: {} has no guardian approval mode. Do not run it on a raw, unsandboxed target.",
                self.display_name()
            )
        })
    }

    /// The launch flag the bridge command line carries, if any.
    ///
    pub const fn launch_flag_for(self, policy: ExecutionPolicy) -> Option<&'static str> {
        match self.execution_enforcement(policy) {
            Some(enforcement) => enforcement.launch_flag(),
            None => None,
        }
    }

    /// Harness-specific arguments that start its ACP stdio server.
    pub fn bridge_args(self, policy: ExecutionPolicy) -> Vec<&'static str> {
        let flag = self.launch_flag_for(policy);
        match self {
            Self::Codex | Self::Claude | Self::Muse => Vec::new(),
            Self::Deepseek => vec!["--profile", "acp"],
            Self::Kimi => vec!["acp"],
            Self::Grok => ["agent"].into_iter().chain(flag).chain(["stdio"]).collect(),
        }
    }
}

impl std::str::FromStr for HarnessKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.id() == value)
            .ok_or_else(|| anyhow!("unknown harness kind {value:?}"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProfile {
    /// Whether Mjolnir may select this profile for new work or probe it.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub kind: HarnessKind,
    /// Controller-side source home. A fresh copy is made for each target.
    pub home: PathBuf,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Conservative byte budget for cross-harness transcript compaction.
    /// Bytes avoid pretending Hel has an accurate tokenizer for every model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_bytes: Option<usize>,
}

impl HarnessProfile {
    pub fn home_env(&self) -> &'static str {
        self.kind.home_env()
    }

    pub fn execution_enforcement(&self, policy: ExecutionPolicy) -> Option<ExecutionEnforcement> {
        self.kind.execution_enforcement(policy)
    }

    fn validate(&self, id: &str) -> Result<()> {
        validate_id("profile", id)?;
        if self.kind == HarnessKind::Muse {
            if self.home.file_name().is_none_or(|name| name != "muse") {
                bail!(
                    "Muse profile {id:?} home must end in /muse (its XDG configuration directory)"
                );
            }
            if self.environment.contains_key("XDG_DATA_HOME") {
                bail!("Muse profile {id:?} must not override its managed XDG_DATA_HOME");
            }
        }
        if self.home.as_os_str().is_empty() {
            bail!("profile {id:?} has an empty home path");
        }
        if self
            .environment
            .keys()
            .any(|key| key.trim().is_empty() || key.contains('='))
        {
            bail!("profile {id:?} contains an invalid environment variable name");
        }
        if self.environment.contains_key(self.kind.home_env()) {
            bail!(
                "profile {id:?} must use `home`, not override {} in `environment`",
                self.kind.home_env()
            );
        }
        if self
            .context_window_bytes
            .is_some_and(|bytes| bytes < 32 * 1024)
        {
            bail!("profile {id:?}: `context_window_bytes` must be at least 32768");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRepository {
    /// Stable name within the bundle, used by `primary_repo`.
    pub id: String,
    /// GitHub HTTPS or SSH URL (or `owner/repository` shorthand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// Controller-side repository whose default network remotes seed isolated sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// Safe relative path beneath the target's bundle root.
    pub destination: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBundle {
    /// Repository id used as the ACP session cwd.
    pub primary_repo: String,
    pub repositories: Vec<ProjectRepository>,
}

impl ProjectBundle {
    fn validate(&self, bundle_id: &str) -> Result<()> {
        validate_id("bundle", bundle_id)?;
        if self.repositories.is_empty() {
            bail!("bundle {bundle_id:?} must contain at least one repository");
        }

        let mut ids = BTreeSet::new();
        let mut destinations = Vec::<PathBuf>::new();
        for repository in &self.repositories {
            validate_id("repository", &repository.id)
                .with_context(|| format!("bundle {bundle_id:?}"))?;
            if !ids.insert(repository.id.as_str()) {
                bail!(
                    "bundle {bundle_id:?} contains duplicate repository id {:?}",
                    repository.id
                );
            }
            if repository.github.is_some() == repository.local.is_some() {
                bail!(
                    "bundle {bundle_id:?} repository {:?} must declare exactly one of `github` or `local`",
                    repository.id,
                );
            }
            if repository
                .github
                .as_deref()
                .is_some_and(|source| !is_github_source(source))
            {
                bail!(
                    "bundle {bundle_id:?} repository {:?} is not a supported GitHub source",
                    repository.id,
                );
            }
            if repository
                .local
                .as_deref()
                .is_some_and(|path| !path.is_absolute())
            {
                bail!(
                    "bundle {bundle_id:?} repository {:?} local path must be absolute",
                    repository.id,
                );
            }
            if repository.git_ref.is_some() {
                bail!(
                    "bundle {bundle_id:?} repository {:?}: git_ref is no longer supported; remove it to start from the remote's default branch",
                    repository.id
                );
            }
            validate_relative_destination(&repository.destination).with_context(|| {
                format!(
                    "bundle {bundle_id:?} repository {:?} destination",
                    repository.id
                )
            })?;
            if let Some(existing) = destinations.iter().find(|existing| {
                repository.destination.starts_with(existing)
                    || existing.starts_with(&repository.destination)
            }) {
                bail!(
                    "bundle {bundle_id:?} contains overlapping destinations {} and {}",
                    existing.display(),
                    repository.destination.display()
                );
            }
            destinations.push(repository.destination.clone());
        }
        if !ids.contains(self.primary_repo.as_str()) {
            bail!(
                "bundle {bundle_id:?} primary repository {:?} does not exist",
                self.primary_repo
            );
        }
        Ok(())
    }

    pub fn primary(&self) -> Option<&ProjectRepository> {
        self.repositories
            .iter()
            .find(|repository| repository.id == self.primary_repo)
    }
}

impl ProjectRepository {
    pub fn source_label(&self) -> String {
        self.github
            .clone()
            .or_else(|| self.local.as_ref().map(|path| path.display().to_string()))
            .unwrap_or_else(|| "invalid repository source".into())
    }

    pub fn is_local(&self) -> bool {
        self.local.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerTemplate {
    pub image: String,
    #[serde(default, skip_serializing_if = "ImagePullPolicy::is_auto")]
    pub pull_policy: ImagePullPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "PodmanWorkspaceStorage::is_default")]
    pub workspace_storage: PodmanWorkspaceStorage,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PodmanWorkspaceStorage {
    #[default]
    PodmanVolume,
    HostHelper {
        root: PathBuf,
        helper: Vec<String>,
    },
    ContainerLayer,
}

impl PodmanWorkspaceStorage {
    fn is_default(&self) -> bool {
        matches!(self, Self::PodmanVolume)
    }

    fn validate(&self, template_id: &str) -> Result<()> {
        let Self::HostHelper { root, helper } = self else {
            return Ok(());
        };
        if !root.is_absolute() {
            bail!("target template {template_id:?} workspace storage root must be absolute");
        }
        if helper.is_empty() || helper.iter().any(|argument| argument.is_empty()) {
            bail!(
                "target template {template_id:?} workspace storage helper must contain non-empty arguments"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImagePullPolicy {
    #[default]
    Auto,
    Always,
    Newer,
    Missing,
    Never,
}

impl ImagePullPolicy {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

impl ContainerTemplate {
    fn validate(&self, template_id: &str) -> Result<()> {
        if self.image.trim().is_empty() {
            bail!("target template {template_id:?} has an empty container image");
        }
        validate_environment(template_id, &self.environment)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AwsAddressSource {
    #[default]
    PublicDns,
    PublicIp,
    PrivateDns,
    PrivateIp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConnection {
    /// OpenSSH destination such as `builder.example.com` or an SSH config alias.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl SshConnection {
    fn validate(&self, template_id: &str) -> Result<()> {
        if self.host.trim().is_empty() || self.host.chars().any(char::is_whitespace) {
            bail!("target template {template_id:?} has an invalid SSH host");
        }
        if self.user.as_deref().is_some_and(|user| {
            user.is_empty() || user.chars().any(|c| c.is_whitespace() || c == '@')
        }) {
            bail!("target template {template_id:?} has an invalid SSH user");
        }
        Ok(())
    }
}

fn default_named_machine_prefix() -> PathBuf {
    PathBuf::from(".local/share/hel/workspaces")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TargetTemplate {
    LocalBare,
    LocalPodman {
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    LocalDocker {
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    AppleContainer {
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    AwsEc2 {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aws_profile: Option<String>,
        region: String,
        launch_template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_template_version: Option<String>,
        ssh_user: String,
        #[serde(default)]
        address_source: AwsAddressSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ssh_args: Vec<String>,
    },
    SshBare {
        #[serde(flatten)]
        ssh: SshConnection,
        permissions: PermissionMode,
        #[serde(default = "default_named_machine_prefix")]
        workspace_prefix: PathBuf,
    },
    SshPodman {
        #[serde(flatten)]
        ssh: SshConnection,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
    SshDocker {
        #[serde(flatten)]
        ssh: SshConnection,
        #[serde(flatten)]
        container: ContainerTemplate,
    },
}

impl TargetTemplate {
    pub const fn execution_policy(&self) -> ExecutionPolicy {
        match self {
            Self::LocalBare => ExecutionPolicy::ConfiguredApprovals,
            Self::SshBare { permissions, .. } => permissions.execution_policy(),
            _ => ExecutionPolicy::Unconstrained,
        }
    }

    pub const fn permission_mode(&self) -> Option<PermissionMode> {
        match self {
            Self::SshBare { permissions, .. } => Some(*permissions),
            _ => None,
        }
    }

    fn validate(&self, id: &str) -> Result<()> {
        validate_id("target template", id)?;
        match self {
            Self::LocalBare => Ok(()),
            Self::LocalPodman { container } => {
                container.validate(id)?;
                container.workspace_storage.validate(id)
            }
            Self::LocalDocker { container } | Self::AppleContainer { container } => {
                container.validate(id)?;
                if !container.workspace_storage.is_default() {
                    bail!("target template {id:?} workspace storage is only supported by Podman");
                }
                Ok(())
            }
            Self::AwsEc2 {
                aws_profile,
                region,
                launch_template,
                launch_template_version,
                ssh_user,
                ..
            } => {
                if region.trim().is_empty()
                    || launch_template.trim().is_empty()
                    || ssh_user.trim().is_empty()
                {
                    bail!(
                        "AWS target template {id:?} requires region, launch_template, and ssh_user"
                    );
                }
                if aws_profile.as_deref().is_some_and(str::is_empty)
                    || launch_template_version
                        .as_deref()
                        .is_some_and(str::is_empty)
                {
                    bail!("AWS target template {id:?} contains an empty optional value");
                }
                Ok(())
            }
            Self::SshBare {
                ssh,
                workspace_prefix,
                ..
            } => {
                ssh.validate(id)?;
                if workspace_prefix.as_os_str().is_empty()
                    || workspace_prefix
                        .components()
                        .any(|part| part == Component::ParentDir)
                    || matches!(workspace_prefix.to_str(), Some("/" | "." | "~" | "~/"))
                {
                    bail!("target template {id:?} has an unsafe workspace prefix");
                }
                Ok(())
            }
            Self::SshDocker { ssh, container } => {
                ssh.validate(id)?;
                container.validate(id)?;
                if !container.workspace_storage.is_default() {
                    bail!("target template {id:?} workspace storage is only supported by Podman");
                }
                Ok(())
            }
            Self::SshPodman { ssh, container, .. } => {
                ssh.validate(id)?;
                container.validate(id)?;
                container.workspace_storage.validate(id)
            }
        }
    }
}

/// Whether `template` hosts a raw project checkout directly on its machine,
/// with no managed workspace. Bare targets take a project directory instead
/// of a bundle.
pub fn is_bare_project_target(template: &TargetTemplate) -> bool {
    matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    )
}

/// The host name that prompt-history mounts on `template` should be filed
/// under, or `None` if the target does not support attached mounts.
pub fn mount_history_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. }
        | TargetTemplate::AwsEc2 { .. } => Some("local"),
        TargetTemplate::SshPodman { ssh, .. } | TargetTemplate::SshDocker { ssh, .. } => {
            Some(&ssh.host)
        }
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. } => None,
    }
}

/// The host key whose project-directory history belongs to `template`.
/// Unlike mount history, raw bare targets keep a recent project checkout list
/// because their launch wizard selects a directory instead of an attachment.
pub fn project_history_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalBare => Some("local"),
        TargetTemplate::SshBare { ssh, .. } => Some(&ssh.host),
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. }
        | TargetTemplate::AwsEc2 { .. }
        | TargetTemplate::SshPodman { .. }
        | TargetTemplate::SshDocker { .. } => None,
    }
}

/// Stable physical-host key for reusable container CPU and memory defaults.
pub fn container_size_host(template: &TargetTemplate) -> Option<&str> {
    match template {
        TargetTemplate::LocalPodman { .. }
        | TargetTemplate::LocalDocker { .. }
        | TargetTemplate::AppleContainer { .. } => Some("local"),
        TargetTemplate::SshPodman { ssh, .. } | TargetTemplate::SshDocker { ssh, .. } => {
            Some(&ssh.host)
        }
        TargetTemplate::LocalBare
        | TargetTemplate::SshBare { .. }
        | TargetTemplate::AwsEc2 { .. } => None,
    }
}

fn validate_environment(owner: &str, environment: &BTreeMap<String, String>) -> Result<()> {
    if environment
        .keys()
        .any(|key| key.trim().is_empty() || key.contains('='))
    {
        bail!("{owner:?} contains an invalid environment variable name");
    }
    Ok(())
}

/// Defaults for new sessions and an empty terminal workspace's first session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupConfig {
    /// Focus the normal composer when the new session is ready.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub prompt: bool,
    /// Create a first session when opening an empty terminal workspace.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl Default for StartupConfig {
    fn default() -> Self {
        Self {
            prompt: true,
            enabled: true,
            profile: None,
            target: None,
        }
    }
}

impl StartupConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn validate(
        &self,
        profiles: &BTreeMap<String, HarnessProfile>,
        targets: &BTreeMap<String, TargetTemplate>,
    ) -> Result<()> {
        if let Some(profile) = &self.profile
            && !profiles.contains_key(profile)
        {
            bail!("startup profile {profile:?} is not configured");
        }
        if let Some(profile) = &self.profile
            && profiles
                .get(profile)
                .is_some_and(|profile| !profile.enabled)
        {
            bail!("startup profile {profile:?} is disabled");
        }
        if let Some(target) = &self.target
            && !targets.contains_key(target)
        {
            bail!("startup target {target:?} is not configured");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SpinnerStyle {
    /// A bright dot glides across a faint row (typing-indicator feel).
    Pulse,
    /// An undulating braille ribbon rolls across the strip.
    Wave,
    /// Vertical bars bounce like an audio equalizer.
    Bars,
    /// The whole row breathes brightness in unison (calmest).
    Shimmer,
    /// A lit sphere rotates in place, carrying its dark side into view.
    Globe,
    /// A lit head sweeps to one wall and back, trailing a fading tail.
    #[default]
    Scan,
}

impl SpinnerStyle {
    pub const ALL: [Self; 6] = [
        Self::Pulse,
        Self::Wave,
        Self::Bars,
        Self::Shimmer,
        Self::Globe,
        Self::Scan,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pulse => "pulse",
            Self::Wave => "wave",
            Self::Bars => "bars",
            Self::Shimmer => "shimmer",
            Self::Globe => "globe",
            Self::Scan => "scan",
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Next animation in the command palette's stable cycle.
    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|style| *style == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

impl std::fmt::Display for SpinnerStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SpinnerStyle {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pulse" => Ok(Self::Pulse),
            "wave" => Ok(Self::Wave),
            "bars" => Ok(Self::Bars),
            "shimmer" => Ok(Self::Shimmer),
            "globe" => Ok(Self::Globe),
            "scan" => Ok(Self::Scan),
            _ => Err(format!(
                "unknown spinner {value:?}; expected one of: {}",
                Self::ALL
                    .iter()
                    .map(|style| style.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Color palette for the terminal dashboard and conversation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiTheme {
    #[default]
    Midnight,
    Light,
    #[serde(rename = "darcula", alias = "dracula")]
    Darcula,
    HighContrast,
}

impl UiTheme {
    pub const ALL: [Self; 4] = [
        Self::Midnight,
        Self::Light,
        Self::Darcula,
        Self::HighContrast,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Midnight => "Midnight",
            Self::Light => "Light",
            Self::Darcula => "Darcula",
            Self::HighContrast => "High Contrast",
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionsSide {
    #[default]
    Left,
    Right,
}

impl SessionsSide {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Settings that are useful while diagnosing or tuning the client surface.
///
/// The section is optional on disk so configurations written before it was
/// introduced retain their existing representation and behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdvancedConfig {
    #[serde(skip_serializing_if = "is_false")]
    pub detailed_activity_clocks: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub show_stopped_sessions: bool,
}

impl AdvancedConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelConfig {
    /// Deprecated session filtering preference retained for read compatibility.
    /// It is ignored and omitted from newly written configurations.
    #[serde(default, skip_serializing)]
    pub show_stopped_sessions: bool,
    #[serde(default, skip_serializing_if = "SessionsSide::is_default")]
    pub sessions_side: SessionsSide,
    #[serde(default, skip_serializing_if = "AdvancedConfig::is_default")]
    pub advanced: AdvancedConfig,
    pub version: u32,
    /// The version found on disk when it was above this build's
    /// [`CONFIG_VERSION`]. Such a config loads best-effort so its settings
    /// still work, and it is read-only: [`HelConfig::save_to`] refuses, so an
    /// older Hel never overwrites a file a newer Mjolnir maintains.
    #[serde(skip)]
    pub newer_config_version: Option<u32>,
    /// Client-side activity animation; omitted configurations retain the classic scan.
    #[serde(default, skip_serializing_if = "SpinnerStyle::is_default")]
    pub spinner: SpinnerStyle,
    #[serde(default, skip_serializing_if = "UiTheme::is_default")]
    pub theme: UiTheme,
    #[serde(default, skip_serializing_if = "PhoneConfig::is_default")]
    pub phone: PhoneConfig,
    #[serde(default, skip_serializing_if = "ReviewConfig::is_default")]
    pub review: ReviewConfig,
    #[serde(default, skip_serializing_if = "StartupConfig::is_default")]
    pub startup: StartupConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, HarnessProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bundles: BTreeMap<String, ProjectBundle>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, TargetTemplate>,
}

impl Default for HelConfig {
    fn default() -> Self {
        Self {
            sessions_side: SessionsSide::default(),
            advanced: AdvancedConfig::default(),
            show_stopped_sessions: false,
            version: CONFIG_VERSION,
            newer_config_version: None,
            spinner: SpinnerStyle::default(),
            theme: Default::default(),
            phone: PhoneConfig::default(),
            review: ReviewConfig::default(),
            startup: StartupConfig::default(),
            profiles: BTreeMap::new(),
            bundles: BTreeMap::new(),
            targets: BTreeMap::new(),
        }
    }
}

impl HelConfig {
    pub fn is_unconfigured(&self) -> bool {
        self.profiles.is_empty() && self.bundles.is_empty() && self.targets.is_empty()
    }

    /// Profiles available for new work, user-facing selectors, and probes.
    pub fn enabled_profiles(&self) -> impl Iterator<Item = (&str, &HarnessProfile)> {
        self.profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, profile)| (id.as_str(), profile))
    }

    /// One profile when it exists and is available for new work.
    pub fn enabled_profile(&self, id: &str) -> Option<&HarnessProfile> {
        self.profiles.get(id).filter(|profile| profile.enabled)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported Mjolnir config version {}; expected {CONFIG_VERSION}",
                self.version
            );
        }
        self.phone.validate()?;
        for (id, profile) in &self.profiles {
            profile.validate(id)?;
        }
        // Checked after the profiles, so a review pointing at a malformed
        // profile reports the profile's own error first.
        self.review.validate(&self.profiles)?;
        self.startup.validate(&self.profiles, &self.targets)?;
        for (id, bundle) in &self.bundles {
            bundle.validate(id)?;
        }
        for (id, target) in &self.targets {
            target.validate(id)?;
        }
        Ok(())
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    /// Read the config from `path`, returning [`HelConfig::default`] when the
    /// file is missing or empty and an error when it is malformed.
    ///
    /// A file written by a *newer* Hel loads best-effort and read-only rather
    /// than refusing to start: its settings still work, and every write path
    /// refuses, so nothing downgrades the file.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path)
            .with_context(|| format!("read Mjolnir config {}", path.display()))?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        let document: toml::Value = contents
            .parse()
            .with_context(|| format!("parse Mjolnir config {}", path.display()))?;
        if let Some(found) = newer_version(&document) {
            tracing::warn!(
                path = %path.display(),
                found_version = found,
                supported_version = CONFIG_VERSION,
                "Mjolnir config was written by a newer build; loading it read-only"
            );
            return Ok(Self::load_newer(&contents, &document, found));
        }
        reject_removed_profile_overrides(&contents)?;
        reject_non_bare_permissions(&contents)?;
        let mut config: Self = toml::from_str(&contents)
            .with_context(|| format!("parse Mjolnir config {}", path.display()))?;
        // Version 2 adds Podman workspace storage; version 3 restores the
        // spinner preference; version 4 adds stopped-session visibility;
        // version 5 adds the terminal theme preference; version 6 adds
        // optional advanced settings; version 7 restores stopped-session
        // visibility as an advanced setting; version 8 lets profiles be
        // disabled. Earlier configs acquire defaults in memory and upgrade on
        // the next ordinary save.
        if matches!(config.version, 1..=7) {
            config.version = CONFIG_VERSION;
        }
        config.validate()?;
        Ok(config)
    }

    /// Best-effort read of a config a newer Mjolnir maintains. Fields this build
    /// does not know drop away, and a section it cannot read falls back on its
    /// own instead of costing the whole file, so the profiles, bundles, and
    /// targets that still parse keep working. The recorded version is what
    /// makes the result read-only.
    fn load_newer(contents: &str, document: &toml::Value, found: u32) -> Self {
        let parsed = toml::from_str::<Self>(contents).ok().map(|mut config| {
            config.version = CONFIG_VERSION;
            config
        });
        let mut config = match parsed {
            Some(config) if config.validate().is_ok() => config,
            _ => Self::salvage(document),
        };
        config.newer_config_version = Some(found);
        config
    }

    /// Recover each section on its own when the document as a whole no longer
    /// matches this build's schema. Maps recover entry by entry, so one target
    /// written in a future shape costs only that target.
    fn salvage(document: &toml::Value) -> Self {
        let mut config = Self::default();
        if let Some(side) = salvage_section::<SessionsSide>(document, "sessions_side") {
            config.sessions_side = side;
        }
        if let Some(theme) = salvage_section::<UiTheme>(document, "theme") {
            config.theme = theme;
        }
        if let Some(spinner) = salvage_section::<SpinnerStyle>(document, "spinner") {
            config.spinner = spinner;
        }
        if let Some(advanced) = salvage_section::<AdvancedConfig>(document, "advanced") {
            config.advanced = advanced;
        }
        if let Some(phone) = salvage_section::<PhoneConfig>(document, "phone")
            && phone.validate().is_ok()
        {
            config.phone = phone;
        }
        config.profiles = salvage_map(document, "profiles", HarnessProfile::validate);
        // Salvaged after the profiles, because whether a review section is
        // usable depends on which profiles survived.
        if let Some(review) = salvage_section::<ReviewConfig>(document, "review")
            && review.validate(&config.profiles).is_ok()
        {
            config.review = review;
        }
        config.bundles = salvage_map(document, "bundles", ProjectBundle::validate);
        config.targets = salvage_map(document, "targets", TargetTemplate::validate);
        if let Some(startup) = salvage_section::<StartupConfig>(document, "startup")
            && startup.validate(&config.profiles, &config.targets).is_ok()
        {
            config.startup = startup;
        }
        config
    }

    /// One line for surfaces that show this config when the file on disk
    /// belongs to a newer Mjolnir; `None` for a config this build owns.
    pub fn newer_build_notice(&self) -> Option<String> {
        self.newer_config_version.map(|found| {
            format!(
                "This config was written by a newer Mjolnir (config version {found}; this build \
                 supports {CONFIG_VERSION}), so it is read-only. Update Mjolnir, or change settings \
                 with the newer build."
            )
        })
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Load the latest config, apply one edit, validate it, and save it while
    /// holding the config lock for the complete transaction.
    ///
    /// The returned config is the version that was written, and the second
    /// value is whatever the edit returned. Keeping the load and edit under
    /// the same lock is what lets independent processes update disjoint
    /// sections without one stale full-config save erasing the other.
    pub fn update<T, F>(edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        Self::update_to(&config_path(), edit)
    }

    /// As [`Self::update`], using an explicit config path.
    pub fn update_to<T, F>(path: &Path, edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        let _lock = ConfigLock::acquire(path)?;
        let mut config = Self::load_from(path)?;
        config.ensure_writable(path)?;
        let value = edit(&mut config)?;
        config.save_to_locked(path)?;
        Ok((config, value))
    }

    /// Loads the current file, replaces only its global review section, and
    /// writes it atomically. Callers use this for the dashboard editor so a
    /// stale dashboard snapshot cannot overwrite profiles, bundles, targets,
    /// or phone settings changed concurrently by another client.
    pub fn save_review(review: ReviewConfig) -> Result<Self> {
        Self::save_review_to(&config_path(), review)
    }

    /// As [`Self::save_review`], using an explicit path for tests and tools.
    pub fn save_review_to(path: &Path, review: ReviewConfig) -> Result<Self> {
        let (config, ()) = Self::update_to(path, |config| {
            config.review = review;
            Ok(())
        })?;
        Ok(config)
    }

    /// Refuses when the file belongs to a newer Mjolnir -- judged by the marker
    /// this config loaded with *and* a fresh look at the file, since a newer
    /// build may have written it since. Overwriting would silently drop
    /// settings this build cannot represent.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let _lock = ConfigLock::acquire(path)?;
        self.save_to_locked(path)
    }

    /// Save while the caller already owns [`ConfigLock`]. This is separate
    /// from [`Self::save_to`] so a transaction does not try to lock the same
    /// sibling file recursively.
    fn save_to_locked(&self, path: &Path) -> Result<()> {
        self.ensure_writable(path)?;
        self.validate()?;
        let body = toml::to_string_pretty(self).context("serialize Mjolnir config")?;
        atomic_write(path, body.as_bytes())
    }

    /// Rename the setup-generated local bare target without rewriting
    /// unrelated configuration. This runs under the controller store lock
    /// before SQLite is opened, so config and persisted sessions converge in
    /// one startup.
    pub fn migrate_legacy_localhost_target() -> Result<bool> {
        Self::migrate_legacy_localhost_target_at(&config_path())
    }

    fn migrate_legacy_localhost_target_at(path: &Path) -> Result<bool> {
        let _lock = ConfigLock::acquire(path)?;
        if !path.exists() {
            return Ok(false);
        }
        let mut config = Self::load_from(path)?;
        if config.newer_config_version.is_some() {
            // The newer Mjolnir that owns this file renames its own targets.
            tracing::warn!(
                path = %path.display(),
                "skipping the legacy localhost target rename: the config belongs to a newer Mjolnir"
            );
            return Ok(false);
        }
        let Some(legacy) = config.targets.get("raw-localhost").cloned() else {
            return Ok(false);
        };
        if let Some(current) = config.targets.get("localhost")
            && current != &legacy
        {
            bail!(
                "cannot rename target `raw-localhost` to `localhost`: both exist with different configurations"
            );
        }
        config.targets.remove("raw-localhost");
        config.targets.entry("localhost".into()).or_insert(legacy);
        if config.startup.target.as_deref() == Some("raw-localhost") {
            config.startup.target = Some("localhost".into());
        }
        config.save_to_locked(path)?;
        Ok(true)
    }

    fn ensure_writable(&self, path: &Path) -> Result<()> {
        if let Some(found) = self
            .newer_config_version
            .or_else(|| newer_version_on_disk(path))
        {
            bail!(
                "{} was written by a newer Mjolnir (config version {found}; this build writes \
                 {CONFIG_VERSION}). Update Mjolnir, or change settings with the newer build",
                path.display()
            );
        }
        Ok(())
    }
}

/// Cross-process lock for one config path. The lock has a stable inode beside
/// the config because the config itself is replaced atomically after the lock
/// is acquired.
struct ConfigLock {
    _file: File,
}

impl ConfigLock {
    fn acquire(config_path: &Path) -> Result<Self> {
        let lock_path = config_lock_path(config_path);
        let parent = lock_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("create config lock directory {}", parent.display()))?;

        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .with_context(|| format!("open config lock {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("lock config {}", config_path.display()))?;
        Ok(Self { _file: file })
    }
}

fn config_lock_path(path: &Path) -> PathBuf {
    let Some(file_name) = path.file_name() else {
        return path.with_extension("lock");
    };
    let mut lock_name = OsString::from(file_name);
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

impl PhoneConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

fn reject_non_bare_permissions(contents: &str) -> Result<()> {
    let value: toml::Value = contents.parse().context("parse Mjolnir config TOML")?;
    let Some(targets) = value.get("targets").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (id, target) in targets {
        let Some(target) = target.as_table() else {
            continue;
        };
        if target.contains_key("permissions")
            && target.get("kind").and_then(toml::Value::as_str) != Some("ssh-bare")
        {
            bail!("target {id:?} sets `permissions`, which is only valid for ssh-bare targets");
        }
    }
    Ok(())
}

/// The config version in `document` when it is above this build's.
fn newer_version(document: &toml::Value) -> Option<u32> {
    let version = document.get("version")?.as_integer()?;
    (version > i64::from(CONFIG_VERSION)).then(|| u32::try_from(version).unwrap_or(u32::MAX))
}

/// The config version at `path` when it is above this build's. Read
/// tolerantly: a missing or unreadable file never blocks a save.
fn newer_version_on_disk(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    newer_version(&contents.parse::<toml::Value>().ok()?)
}

/// Deserialize one top-level section, or `None` when this build cannot read
/// the shape a newer Mjolnir wrote.
fn salvage_section<T: for<'de> Deserialize<'de>>(document: &toml::Value, key: &str) -> Option<T> {
    document
        .get(key)
        .cloned()
        .and_then(|value| value.try_into().ok())
}

/// Deserialize one top-level table entry by entry, dropping only the entries
/// this build cannot read or accept.
fn salvage_map<T, F>(document: &toml::Value, key: &str, validate: F) -> BTreeMap<String, T>
where
    T: for<'de> Deserialize<'de>,
    F: Fn(&T, &str) -> Result<()>,
{
    let Some(table) = document.get(key).and_then(toml::Value::as_table) else {
        return BTreeMap::new();
    };
    let mut kept = BTreeMap::new();
    for (id, value) in table {
        match value.clone().try_into::<T>() {
            Ok(entry) => match validate(&entry, id) {
                Ok(()) => {
                    kept.insert(id.clone(), entry);
                }
                Err(error) => tracing::warn!(
                    section = key,
                    id,
                    %error,
                    "dropping a newer Mjolnir config entry this build rejects"
                ),
            },
            Err(error) => tracing::warn!(
                section = key,
                id,
                %error,
                "dropping a newer Mjolnir config entry this build cannot read"
            ),
        }
    }
    kept
}

fn reject_removed_profile_overrides(contents: &str) -> Result<()> {
    let value: toml::Value = contents.parse().context("parse Mjolnir config TOML")?;
    let Some(profiles) = value.get("profiles").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (id, profile) in profiles {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        for key in ["model", "reasoning_effort"] {
            if profile.contains_key(key) {
                bail!(
                    "profile {id:?}: `{key}` is no longer supported; configure it in the harness home or change it per session with `/config`"
                );
            }
        }
    }
    Ok(())
}

/// Read a configuration override under its `MJ_` name. Mjolnir shares no
/// state or environment with hel installs; there is no legacy fallback.
pub fn env_override_os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("MJ_{name}"))
}

/// String form of [`env_override_os`] for overrides parsed as UTF-8.
pub fn env_override(name: &str) -> Option<String> {
    std::env::var(format!("MJ_{name}")).ok()
}

pub fn config_dir() -> PathBuf {
    if let Some(path) = env_override_os("CONFIG_DIR") {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join(PRODUCT_DIR)
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    if let Some(path) = env_override_os("DATA_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join(PRODUCT_DIR)
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

pub fn validate_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || matches!(id, "." | "..")
    {
        bail!("invalid {kind} id {id:?}; use 1-64 ASCII letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

pub fn validate_relative_destination(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("destination must be a non-empty relative path");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => bail!("destination must not contain '.'"),
            Component::ParentDir => bail!("destination must not contain '..'"),
            Component::Prefix(_) | Component::RootDir => {
                bail!("destination must not be absolute")
            }
        }
    }
    Ok(())
}

fn is_github_source(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() || source.starts_with('-') || source.chars().any(char::is_whitespace) {
        return false;
    }
    let repository_path = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("git@github.com:"))
        .or_else(|| source.strip_prefix("ssh://git@github.com/"))
        .unwrap_or(source);
    let mut parts = repository_path.trim_end_matches(".git").split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repository), None) if !owner.is_empty() && !repository.is_empty())
}

/// Replace `path` without exposing a partially-written configuration/state file.
pub fn atomic_write(path: &Path, body: &[u8]) -> Result<()> {
    atomic_write_with_parent(path, body, ParentDirectory::Create)
}

/// Replace `path` only while its directory still exists.
///
/// Worker state lives inside a directory that session teardown deletes out
/// from under the running daemon. Recreating it here would resurrect a closed
/// session's relay state, so a vanished parent must be an error instead.
pub fn atomic_write_existing(path: &Path, body: &[u8]) -> Result<()> {
    atomic_write_with_parent(path, body, ParentDirectory::Require)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentDirectory {
    Create,
    Require,
}

fn atomic_write_with_parent(
    path: &Path,
    body: &[u8],
    parent_directory: ParentDirectory,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match parent_directory {
        ParentDirectory::Create => {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        ParentDirectory::Require => {
            if !parent.is_dir() {
                bail!("directory {} is missing", parent.display());
            }
        }
    }

    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow!("generate temporary filename: {error}"))?;
    let suffix = u64::from_le_bytes(random);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hel");
    let temporary = parent.join(format!(
        ".{file_name}.{}.{suffix:016x}.tmp",
        std::process::id()
    ));

    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(body)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, path)
            .with_context(|| format!("replace {} with {}", path.display(), temporary.display()))?;
        #[cfg(unix)]
        OpenOptions::new()
            .read(true)
            .open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_defaults_round_trip_and_validate_the_selected_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        config.save_to(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("[startup]"));
        assert_eq!(
            HelConfig::load_from(&path).unwrap().startup,
            StartupConfig::default()
        );
        config.startup.profile = Some("codex-1".into());
        config.startup.target = config.targets.keys().next().cloned();
        config.startup.enabled = false;
        config.save_to(&path).unwrap();
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);
        config.startup.profile = Some("missing".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("startup profile")
        );
        config.startup.profile = None;
        config.startup.target = Some("missing".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("startup target")
        );
    }

    #[test]
    fn stopped_session_visibility_defaults_off_and_uses_the_advanced_section() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let legacy = "version = 6\nshow_stopped_sessions = true\n";
        fs::write(&path, legacy).unwrap();
        let config = HelConfig::load_from(&path).unwrap();
        assert!(config.show_stopped_sessions);
        assert!(!config.advanced.show_stopped_sessions);
        assert_eq!(fs::read_to_string(&path).unwrap(), legacy);

        config.save_to(&path).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(!body.contains("show_stopped_sessions"));

        let (saved, ()) = HelConfig::update_to(&path, |config| {
            config.advanced.show_stopped_sessions = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(HelConfig::load_from(&path).unwrap(), saved);
        assert!(saved.advanced.show_stopped_sessions);
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("[advanced]"));
        assert!(body.contains("show_stopped_sessions = true"));
        assert_eq!(saved.version, CONFIG_VERSION);
    }

    #[test]
    fn muse_home_mapping_keeps_config_credentials_and_session_data_together() {
        let home = Path::new("/private/session/muse");
        let mut environment = BTreeMap::from([("XDG_DATA_HOME".into(), "/unrelated".into())]);
        HarnessKind::Muse.configure_home_environment(home, &mut environment);
        assert_eq!(environment["XDG_CONFIG_HOME"], "/private/session");
        assert_eq!(environment["XDG_DATA_HOME"], "/private/session/muse/.data");
        assert_eq!(
            HarnessKind::Muse.home_from_environment(&environment["XDG_CONFIG_HOME"]),
            home
        );
        assert_eq!(
            harness_authentication_marker(HarnessKind::Muse, home),
            home.join("auth.json")
        );
    }

    #[test]
    fn codex_target_policy_selects_mode_without_replacing_host_config() {
        for (policy, mode) in [
            (ExecutionPolicy::ConfiguredApprovals, "agent"),
            (ExecutionPolicy::Unconstrained, "agent-full-access"),
        ] {
            let config = r#"{"default_permissions":"project","model":"configured-model"}"#;
            let mut environment = BTreeMap::from([("CODEX_CONFIG".into(), config.into())]);
            HarnessKind::Codex
                .configure_execution_environment(policy, &mut environment)
                .unwrap();
            assert_eq!(environment["INITIAL_AGENT_MODE"], mode);
            assert_eq!(environment["CODEX_CONFIG"], config);
        }
    }

    #[test]
    fn muse_guardian_preserves_policy_and_unconstrained_launch_is_explicit() {
        let original = BTreeMap::from([
            ("MUSE_APPROVAL_MODE".into(), "ask".into()),
            (
                "MUSE_SERVE_ARGS".into(),
                "--sandbox-network restricted".into(),
            ),
        ]);
        let mut environment = original.clone();
        HarnessKind::Muse
            .configure_execution_environment(ExecutionPolicy::ConfiguredApprovals, &mut environment)
            .unwrap();
        assert_eq!(environment, original);
        HarnessKind::Muse
            .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut environment)
            .unwrap();
        HarnessKind::Muse
            .configure_execution_environment(ExecutionPolicy::Unconstrained, &mut environment)
            .unwrap();
        assert_eq!(environment["MUSE_APPROVAL_MODE"], "auto");
        assert_eq!(
            environment["MUSE_SERVE_ARGS"],
            "--sandbox-network restricted --disable-sandbox"
        );
    }

    fn sample_config() -> HelConfig {
        HelConfig {
            version: CONFIG_VERSION,
            sessions_side: Default::default(),
            advanced: Default::default(),
            show_stopped_sessions: false,
            newer_config_version: None,
            spinner: SpinnerStyle::default(),
            theme: Default::default(),
            phone: PhoneConfig::default(),
            review: ReviewConfig::default(),
            startup: Default::default(),
            profiles: BTreeMap::from([(
                "codex-1".into(),
                HarnessProfile {
                    enabled: true,
                    context_window_bytes: None,
                    kind: HarnessKind::Codex,
                    home: PathBuf::from("/home/test/.codex-one"),
                    environment: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
                },
            )]),
            bundles: BTreeMap::from([(
                "hel".into(),
                ProjectBundle {
                    primary_repo: "app".into(),
                    repositories: vec![ProjectRepository {
                        id: "app".into(),
                        github: Some("BrokkAi/hel".into()),
                        local: None,
                        destination: PathBuf::from("app"),
                        git_ref: None,
                    }],
                },
            )]),
            targets: BTreeMap::from([(
                "podman-default".into(),
                TargetTemplate::LocalPodman {
                    container: ContainerTemplate {
                        image: "ubuntu:24.04".into(),
                        pull_policy: ImagePullPolicy::Auto,
                        platform: None,
                        cpus: None,
                        memory: None,
                        environment: BTreeMap::new(),
                        workspace_storage: Default::default(),
                    },
                },
            )]),
        }
    }

    #[test]
    fn harness_profiles_reject_the_removed_executable_override() {
        let error = toml::from_str::<HarnessProfile>(
            "kind = \"codex\"\nhome = \"/profiles/codex\"\nexecutable = \"/opt/codex-acp\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `executable`"));
    }

    #[test]
    fn legacy_localhost_target_migration_is_atomic_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        config.targets.clear();
        config
            .targets
            .insert("raw-localhost".into(), TargetTemplate::LocalBare);
        config.save_to(&path).unwrap();

        assert!(HelConfig::migrate_legacy_localhost_target_at(&path).unwrap());
        let migrated = HelConfig::load_from(&path).unwrap();
        assert_eq!(
            migrated.targets.get("localhost"),
            Some(&TargetTemplate::LocalBare)
        );
        assert!(!migrated.targets.contains_key("raw-localhost"));
        assert!(!HelConfig::migrate_legacy_localhost_target_at(&path).unwrap());
    }

    #[test]
    fn conflicting_localhost_target_migration_leaves_config_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        config
            .targets
            .insert("raw-localhost".into(), TargetTemplate::LocalBare);
        config.targets.insert(
            "localhost".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    image: "different".into(),
                    pull_policy: ImagePullPolicy::Auto,
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        );
        config.save_to(&path).unwrap();
        let before = fs::read(&path).unwrap();

        assert!(HelConfig::migrate_legacy_localhost_target_at(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn harness_mapping_and_permission_modes_are_fixed() {
        assert_eq!(HarnessKind::Codex.home_env(), "CODEX_HOME");
        assert_eq!(HarnessKind::Claude.home_env(), "CLAUDE_CONFIG_DIR");
        assert_eq!(HarnessKind::Kimi.home_env(), "KIMI_CODE_HOME");
        assert_eq!(HarnessKind::Grok.home_env(), "GROK_HOME");
        let codex = HarnessKind::Codex
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(codex.acp_mode(), Some("agent-full-access"));
        assert_eq!(codex.label(), "agent-full-access");
        let claude = HarnessKind::Claude
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(claude.acp_mode(), Some("bypassPermissions"));
        let kimi = HarnessKind::Kimi
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(kimi.acp_mode(), Some("auto"));
    }

    #[test]
    fn unconstrained_enforcement_splits_acp_modes_from_launch_controls() {
        for kind in [HarnessKind::Codex, HarnessKind::Kimi] {
            let enforcement = kind
                .execution_enforcement(ExecutionPolicy::Unconstrained)
                .unwrap();
            assert_eq!(enforcement.acp_mode(), Some(enforcement.label()));
            assert_eq!(enforcement.launch_flag(), None);
        }
        assert_eq!(
            HarnessKind::Codex
                .execution_enforcement(ExecutionPolicy::Unconstrained)
                .unwrap()
                .launch_environment(),
            Some(("INITIAL_AGENT_MODE", "agent-full-access"))
        );
        let grok = HarnessKind::Grok
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(grok.acp_mode(), None);
        assert_eq!(grok.launch_flag(), Some("--always-approve"));
        assert_eq!(grok.label(), "always-approve / sandbox-off");
        assert_eq!(grok.launch_environment(), Some(("GROK_SANDBOX", "off")));
        let claude = HarnessKind::Claude
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(claude.acp_mode(), Some("bypassPermissions"));
        assert_eq!(claude.label(), "bypassPermissions / sandbox-off");
        let deepseek = HarnessKind::Deepseek
            .execution_enforcement(ExecutionPolicy::Unconstrained)
            .unwrap();
        assert_eq!(deepseek.acp_mode(), None);
        assert_eq!(deepseek.launch_flag(), None);
        assert_eq!(
            deepseek.launch_environment(),
            Some(("DSH_PERMISSION_MODE", "danger-full-access"))
        );
    }

    #[test]
    fn configured_approvals_preserve_other_profiles_and_select_codex_guardian() {
        let codex = HarnessKind::Codex
            .execution_enforcement(ExecutionPolicy::ConfiguredApprovals)
            .expect("Codex ACP selects guardian explicitly");
        assert_eq!(codex.acp_mode(), Some("agent"));
        assert_eq!(
            codex.launch_environment(),
            Some(("INITIAL_AGENT_MODE", "agent"))
        );

        for kind in [
            HarnessKind::Claude,
            HarnessKind::Kimi,
            HarnessKind::Grok,
            HarnessKind::Deepseek,
        ] {
            assert_eq!(
                kind.execution_enforcement(ExecutionPolicy::ConfiguredApprovals),
                None,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn harness_names_and_ids_round_trip() {
        for kind in HarnessKind::ALL {
            assert_eq!(kind.id().parse::<HarnessKind>().unwrap(), kind);
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.id().to_owned())
            );
            assert!(!kind.display_name().is_empty());
            assert!(kind.default_home_leaf().starts_with('.'));
        }
        assert_eq!(HarnessKind::Grok.id(), "grok");
        assert_eq!(HarnessKind::Grok.display_name(), "Grok Build");
        assert_eq!(HarnessKind::Grok.default_home_leaf(), ".grok");
        assert_eq!(HarnessKind::Deepseek.display_name(), "DSH");
        assert_eq!(HarnessKind::Deepseek.home_env(), "DSH_HOME");
        assert!("nope".parse::<HarnessKind>().is_err());
    }

    #[test]
    fn bridge_args_carry_the_acp_subcommand_per_harness() {
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            assert!(HarnessKind::Codex.bridge_args(policy).is_empty());
            assert!(HarnessKind::Claude.bridge_args(policy).is_empty());
            assert_eq!(HarnessKind::Kimi.bridge_args(policy), ["acp"]);
            assert_eq!(
                HarnessKind::Deepseek.bridge_args(policy),
                ["--profile", "acp"]
            );
            assert_eq!(
                HarnessKind::Grok.bridge_args(policy),
                if policy.is_unconstrained() {
                    vec!["agent", "--always-approve", "stdio"]
                } else {
                    vec!["agent", "stdio"]
                },
                "policy: {policy:?}"
            );
        }
    }

    #[test]
    fn only_unconstrained_grok_carries_the_blanket_approval_flag() {
        assert_eq!(
            HarnessKind::Grok.launch_flag_for(ExecutionPolicy::ConfiguredApprovals),
            None
        );
        assert_eq!(
            HarnessKind::Grok.launch_flag_for(ExecutionPolicy::Unconstrained),
            Some("--always-approve")
        );
        for kind in [
            HarnessKind::Codex,
            HarnessKind::Claude,
            HarnessKind::Kimi,
            HarnessKind::Deepseek,
        ] {
            for policy in [
                ExecutionPolicy::ConfiguredApprovals,
                ExecutionPolicy::Unconstrained,
            ] {
                assert_eq!(kind.launch_flag_for(policy), None, "{kind:?}");
            }
        }
    }

    #[test]
    fn guardian_support_is_declared_per_harness() {
        for kind in [HarnessKind::Codex, HarnessKind::Claude, HarnessKind::Grok] {
            assert!(kind.supports_guardian_approvals(), "{kind:?}");
        }
        for kind in [HarnessKind::Kimi, HarnessKind::Deepseek] {
            assert!(!kind.supports_guardian_approvals(), "{kind:?}");
        }
    }

    #[test]
    fn bundle_rejects_traversal_and_duplicate_destinations() {
        let mut config = sample_config();
        config.bundles.get_mut("hel").unwrap().repositories[0].destination =
            PathBuf::from("../escape");
        assert!(format!("{:#}", config.validate().unwrap_err()).contains("'..'"));

        let mut config = sample_config();
        let bundle = config.bundles.get_mut("hel").unwrap();
        bundle.repositories.push(ProjectRepository {
            id: "docs".into(),
            github: Some("BrokkAi/docs".into()),
            local: None,
            destination: PathBuf::from("app"),
            git_ref: None,
        });
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("overlapping destinations")
        );
    }

    #[test]
    fn bundle_requires_existing_primary_repository() {
        let mut config = sample_config();
        config.bundles.get_mut("hel").unwrap().primary_repo = "missing".into();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
    }

    #[test]
    fn bundle_rejects_non_github_sources() {
        let mut config = sample_config();
        config.bundles.get_mut("hel").unwrap().repositories[0].github =
            Some("https://example.com/owner/repo".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not a supported GitHub source")
        );
    }

    #[test]
    fn bundle_accepts_one_absolute_local_source() {
        let mut config = sample_config();
        {
            let repository = &mut config.bundles.get_mut("hel").unwrap().repositories[0];
            repository.github = None;
            repository.local = Some(PathBuf::from("/home/test/src/app"));
        }
        config.validate().unwrap();

        config.bundles.get_mut("hel").unwrap().repositories[0].local =
            Some(PathBuf::from("relative/app"));
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
    }

    #[test]
    fn bundle_requires_exactly_one_repository_source() {
        let mut config = sample_config();
        config.bundles.get_mut("hel").unwrap().repositories[0].local =
            Some(PathBuf::from("/home/test/src/app"));
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn config_toml_round_trip_is_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");
        let config = sample_config();
        config.save_to(&path).unwrap();
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);
        assert!(!fs::read_to_string(&path).unwrap().contains("pull_policy"));
        assert_eq!(
            fs::read_to_string(path)
                .unwrap()
                .matches("kind = \"local-podman\"")
                .count(),
            1
        );
        assert!(
            fs::read_dir(directory.path().join("nested"))
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
        );
    }

    #[test]
    fn save_review_reloads_latest_config_and_preserves_unrelated_sections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let initial = sample_config();
        initial.save_to(&path).unwrap();

        // Simulate a concurrent dashboard changing an unrelated section after
        // the editor opened. The review save must start from this newer file.
        let mut latest = HelConfig::load_from(&path).unwrap();
        latest.phone.enabled = false;
        latest
            .profiles
            .get_mut("codex-1")
            .unwrap()
            .environment
            .insert("LATEST_SETTING".into(), "kept".into());
        latest.save_to(&path).unwrap();

        let review = ReviewConfig {
            enabled: true,
            tier: crate::hel_review::lanes::ReviewTier::Extended,
            profile: Some("codex-1".into()),
            model: Some("review-model".into()),
            effort: Some("high".into()),
        };
        let saved = HelConfig::save_review_to(&path, review.clone()).unwrap();
        assert_eq!(saved.review, review);
        assert!(!saved.phone.enabled);
        assert_eq!(
            saved.profiles["codex-1"].environment.get("LATEST_SETTING"),
            Some(&"kept".to_owned())
        );
        assert_eq!(HelConfig::load_from(&path).unwrap(), saved);
    }

    #[test]
    fn update_to_serializes_disjoint_process_edits() {
        const CHILD: &str = "HEL_CONFIG_UPDATE_CHILD";
        const PATH: &str = "HEL_CONFIG_UPDATE_PATH";
        const READY: &str = "HEL_CONFIG_UPDATE_READY";
        const SECOND_STARTED: &str = "HEL_CONFIG_UPDATE_SECOND_STARTED";
        const RELEASE: &str = "HEL_CONFIG_UPDATE_RELEASE";
        let Some(role) = std::env::var_os(CHILD) else {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            sample_config().save_to(&path).unwrap();
            let ready = directory.path().join("ready");
            let second_started = directory.path().join("second-started");
            let release = directory.path().join("release");

            let executable = std::env::current_exe().unwrap();
            let mut first = std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "hel_config::tests::update_to_serializes_disjoint_process_edits",
                    "--nocapture",
                ])
                .env(CHILD, "phone")
                .env(PATH, &path)
                .env(READY, &ready)
                .env(SECOND_STARTED, &second_started)
                .env(RELEASE, &release)
                .spawn()
                .unwrap();
            // The first child writes this only after it has acquired the
            // sibling lock and entered its edit closure.
            let first_entered = (0..1000).any(|_| {
                if ready.exists() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }) || ready.exists();

            let mut second = std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "hel_config::tests::update_to_serializes_disjoint_process_edits",
                    "--nocapture",
                ])
                .env(CHILD, "profile")
                .env(PATH, &path)
                .env(READY, &ready)
                .env(SECOND_STARTED, &second_started)
                .env(RELEASE, &release)
                .spawn()
                .unwrap();
            let second_reached_update = (0..1000).any(|_| {
                if second_started.exists() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }) || second_started.exists();
            let second_blocked = second.try_wait().unwrap().is_none();
            // Always release the first child before asserting, so a failed
            // observation cannot leave a child waiting after this test exits.
            fs::write(&release, b"release").unwrap();
            let first_status = first.wait().unwrap();
            let second_status = second.wait().unwrap();
            assert!(first_entered, "first config update child never entered");
            assert!(
                second_reached_update,
                "second config update child never reached its update"
            );
            assert!(second_blocked, "second config update child was not blocked");
            assert!(
                first_status.success(),
                "first config update child failed: {first_status}"
            );
            assert!(
                second_status.success(),
                "second config update child failed: {second_status}"
            );

            let config = HelConfig::load_from(&path).unwrap();
            assert!(!config.phone.enabled);
            assert_eq!(
                config.profiles["codex-1"].environment.get("CONCURRENT"),
                Some(&"kept".to_owned())
            );
            return;
        };

        let path = PathBuf::from(std::env::var_os(PATH).unwrap());
        let role = role.to_string_lossy();
        let ready = PathBuf::from(std::env::var_os(READY).unwrap());
        let second_started = PathBuf::from(std::env::var_os(SECOND_STARTED).unwrap());
        let release = PathBuf::from(std::env::var_os(RELEASE).unwrap());
        if role == "profile" {
            // Confirm the first process owns the stable lock before telling
            // the parent it can release it. This cannot pass merely because
            // the second process was slow to reach update_to.
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(config_lock_path(&path))
                .unwrap();
            assert!(matches!(
                lock.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            fs::write(&second_started, b"started").unwrap();
        }
        HelConfig::update_to(&path, |config| {
            match role.as_ref() {
                "phone" => {
                    fs::write(&ready, b"entered").unwrap();
                    while !release.exists() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    config.phone.enabled = false;
                }
                "profile" => {
                    config
                        .profiles
                        .get_mut("codex-1")
                        .unwrap()
                        .environment
                        .insert("CONCURRENT".into(), "kept".into());
                }
                other => panic!("unknown config update child role {other:?}"),
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn update_to_failure_leaves_the_previous_file_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        sample_config().save_to(&path).unwrap();
        let before = fs::read(&path).unwrap();

        let error = HelConfig::update_to(&path, |config| {
            config.phone.bind = "not-an-address".into();
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("parse phone bind"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn update_to_refuses_a_newer_config_before_editing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let body = format!("version = {}\nfuture = true\n", CONFIG_VERSION + 1);
        fs::write(&path, &body).unwrap();

        let error = HelConfig::update_to(&path, |config| {
            config.phone.enabled = false;
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("newer Mjolnir"));
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn version_one_podman_config_upgrades_to_isolated_workspace_storage() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "version = 1\n\n[targets.podman]\nkind = \"local-podman\"\nimage = \"ubuntu:24.04\"\n",
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        let TargetTemplate::LocalPodman { container } = &config.targets["podman"] else {
            panic!("version-one Podman target changed kind")
        };
        assert_eq!(
            container.workspace_storage,
            PodmanWorkspaceStorage::PodmanVolume
        );
        assert!(fs::read_to_string(path).unwrap().starts_with("version = 1"));
    }

    #[test]
    fn old_config_restores_scan_without_rewriting_until_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "version = 2\n").unwrap();

        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.spinner, SpinnerStyle::Scan);
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(fs::read_to_string(&path).unwrap(), "version = 2\n");
        config.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with(&format!("version = {CONFIG_VERSION}")));
        assert!(!saved.contains("spinner"));
    }

    #[test]
    fn detailed_activity_clocks_default_off_and_round_trip_without_breaking_old_configs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "version = 2\n").unwrap();
        let old = HelConfig::load_from(&path).unwrap();
        assert!(!old.advanced.detailed_activity_clocks);

        let mut config = old;
        config.advanced.detailed_activity_clocks = true;
        config.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("[advanced]"));
        assert!(saved.contains("detailed_activity_clocks = true"));
        assert!(
            HelConfig::load_from(&path)
                .unwrap()
                .advanced
                .detailed_activity_clocks
        );
    }

    #[test]
    fn every_previous_config_version_upgrades_with_compatible_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for version in 1..CONFIG_VERSION {
            fs::write(&path, format!("version = {version}\n")).unwrap();
            let config = HelConfig::load_from(&path).unwrap();
            assert_eq!(config.version, CONFIG_VERSION);
            assert!(!config.show_stopped_sessions);
            assert_eq!(config.theme, UiTheme::Midnight);
            assert!(!config.advanced.detailed_activity_clocks);
            assert!(!config.advanced.show_stopped_sessions);
        }
    }

    #[test]
    fn spinner_preferences_round_trip_without_replacing_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        config.phone.enabled = false;
        config.save_to(&path).unwrap();

        for spinner in SpinnerStyle::ALL {
            HelConfig::update_to(&path, |config| {
                config.spinner = spinner;
                Ok(())
            })
            .unwrap();
            let reloaded = HelConfig::load_from(&path).unwrap();
            assert_eq!(reloaded.spinner, spinner);
            assert_eq!(reloaded.phone, config.phone);
            assert_eq!(reloaded.profiles, config.profiles);
        }
    }

    #[test]
    fn theme_preferences_upgrade_and_round_trip_without_replacing_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let old = "version = 4\nshow_stopped_sessions = false\n";
        fs::write(&path, old).unwrap();
        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.theme, UiTheme::Midnight);
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(fs::read_to_string(&path).unwrap(), old);

        for theme in UiTheme::ALL {
            HelConfig::update_to(&path, |config| {
                config.theme = theme;
                Ok(())
            })
            .unwrap();
            let mut expected = config.clone();
            expected.theme = theme;
            assert_eq!(HelConfig::load_from(&path).unwrap(), expected);
        }
    }

    #[test]
    fn legacy_dracula_theme_loads_and_saves_as_darcula() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\ntheme = \"dracula\"\n"),
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.theme, UiTheme::Darcula);
        assert_eq!(UiTheme::ALL.len(), 4);
        config.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("theme = \"darcula\""), "{saved}");
        assert!(!saved.contains("dracula"), "{saved}");
    }

    #[test]
    fn unknown_theme_is_rejected_but_newer_configs_salvage_known_themes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\ntheme = \"unknown\"\n"),
        )
        .unwrap();
        assert!(HelConfig::load_from(&path).is_err());

        let newer = format!(
            "version = {}\ntheme = \"light\"\nfuture = true\n",
            CONFIG_VERSION + 1
        );
        fs::write(&path, &newer).unwrap();
        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.theme, UiTheme::Light);
        assert!(config.save_to(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn explicit_container_layer_and_host_helper_storage_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        if let TargetTemplate::LocalPodman { container } =
            config.targets.get_mut("podman-default").unwrap()
        {
            container.workspace_storage = PodmanWorkspaceStorage::HostHelper {
                root: PathBuf::from("/srv/mj-workspaces"),
                helper: vec!["sudo".into(), "-n".into(), "/opt/mj-helper".into()],
            };
        }
        config.save_to(&path).unwrap();
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);

        if let TargetTemplate::LocalPodman { container } =
            config.targets.get_mut("podman-default").unwrap()
        {
            container.workspace_storage = PodmanWorkspaceStorage::ContainerLayer;
        }
        config.save_to(&path).unwrap();
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);
    }

    #[test]
    fn local_docker_target_round_trips_with_its_public_kind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        let container = match config.targets.remove("podman-default").unwrap() {
            TargetTemplate::LocalPodman { container } => container,
            _ => unreachable!(),
        };
        config
            .targets
            .insert("docker".into(), TargetTemplate::LocalDocker { container });

        config.save_to(&path).unwrap();

        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("kind = \"local-docker\""), "{rendered}");
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);
    }

    #[test]
    fn explicit_image_pull_policy_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        let TargetTemplate::LocalPodman { container } =
            config.targets.get_mut("podman-default").unwrap()
        else {
            unreachable!()
        };
        container.pull_policy = ImagePullPolicy::Never;

        config.save_to(&path).unwrap();

        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("pull_policy = \"never\"")
        );
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);
    }

    #[test]
    fn raw_ssh_permissions_are_required_and_podman_rejects_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = sample_config();
        let container = match config.targets.remove("podman-default").unwrap() {
            TargetTemplate::LocalPodman { container } => container,
            _ => unreachable!(),
        };
        let ssh = SshConnection {
            host: "builder".into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        };
        config.targets = BTreeMap::from([
            (
                "builder-guardian".into(),
                TargetTemplate::SshBare {
                    ssh: ssh.clone(),
                    permissions: PermissionMode::Guardian,
                    workspace_prefix: default_named_machine_prefix(),
                },
            ),
            (
                "builder-yolo".into(),
                TargetTemplate::SshBare {
                    ssh: ssh.clone(),
                    permissions: PermissionMode::Yolo,
                    workspace_prefix: default_named_machine_prefix(),
                },
            ),
            (
                "builder-podman".into(),
                TargetTemplate::SshPodman { ssh, container },
            ),
        ]);

        config.save_to(&path).unwrap();

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("permissions = \"guardian\""), "{body}");
        assert!(body.contains("permissions = \"yolo\""), "{body}");
        assert_eq!(body.matches("permissions = ").count(), 2, "{body}");
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);

        fs::write(
            &path,
            "version = 1\n[targets.builder]\nkind = \"ssh-bare\"\nhost = \"builder\"\n",
        )
        .unwrap();
        let error = format!("{:#}", HelConfig::load_from(&path).unwrap_err());
        assert!(error.contains("permissions"), "{error}");

        fs::write(
            &path,
            "version = 1\n[targets.builder]\nkind = \"ssh-podman\"\nhost = \"builder\"\npermissions = \"guardian\"\nimage = \"example.invalid/agent:latest\"\n",
        )
        .unwrap();
        let error = format!("{:#}", HelConfig::load_from(&path).unwrap_err());
        assert!(error.contains("only valid for ssh-bare"), "{error}");
    }

    #[test]
    fn missing_config_uses_clean_v1_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = HelConfig::load_from(&directory.path().join("missing.toml")).unwrap();
        assert_eq!(config, HelConfig::default());
        assert!(config.phone.enabled);
        assert!(config.phone.tailscale_detect);
    }

    #[test]
    fn omitted_phone_fields_enable_the_web_viewer_and_tailscale_detection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "version = 1\n[phone]\nbind = \"127.0.0.1:4765\"\n").unwrap();

        let config = HelConfig::load_from(&path).unwrap();

        assert!(config.phone.enabled);
        assert!(config.phone.tailscale_detect);
        assert_eq!(config.phone.bind, "127.0.0.1:4765");
    }

    #[test]
    fn version_seven_profiles_upgrade_enabled_and_disabled_round_trips_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "version = 7\n[profiles.work]\nkind = \"codex\"\nhome = \"/profiles/work\"\n",
        )
        .unwrap();

        let mut config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert!(config.profiles["work"].enabled);
        assert_eq!(
            config
                .enabled_profiles()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec!["work"]
        );

        config.save_to(&path).unwrap();
        let enabled = fs::read_to_string(&path).unwrap();
        assert!(enabled.starts_with("version = 8"), "{enabled}");
        assert!(!enabled.contains("enabled = true"), "{enabled}");

        config.profiles.get_mut("work").unwrap().enabled = false;
        config.save_to(&path).unwrap();
        let disabled = fs::read_to_string(&path).unwrap();
        assert!(disabled.contains("enabled = false"), "{disabled}");
        assert!(!HelConfig::load_from(&path).unwrap().profiles["work"].enabled);
    }

    #[test]
    fn startup_and_review_reject_disabled_profile_references() {
        let profile =
            "[profiles.work]\nenabled = false\nkind = \"claude\"\nhome = \"/profiles/work\"\n";
        for reference in [
            "[startup]\nprofile = \"work\"\n",
            "[review]\nprofile = \"work\"\n",
        ] {
            let error = toml::from_str::<HelConfig>(&format!(
                "version = {CONFIG_VERSION}\n{reference}{profile}"
            ))
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
            assert!(error.contains("disabled"), "{error}");
        }
    }

    /// A profile that exists, so a `[review]` section has something to name.
    fn config_with_profile(profile: &str) -> String {
        format!(
            "version = 1\n\n[profiles.{profile}]\nkind = \"claude\"\nhome = \"/home/u/.claude\"\n"
        )
    }

    #[test]
    fn review_is_off_and_quick_until_the_config_says_otherwise() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, config_with_profile("reviewer")).unwrap();

        let config = HelConfig::load_from(&path).unwrap();

        assert!(!config.review.enabled, "review is opt-in");
        assert_eq!(
            config.review.tier,
            crate::hel_review::lanes::ReviewTier::Quick
        );
        assert_eq!(config.review.reviewer_profile(), None);
    }

    #[test]
    fn a_review_section_names_the_profile_that_reviews() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "{}\n[review]\nenabled = true\ntier = \"extended\"\nprofile = \"reviewer\"\nmodel = \"opus\"\n",
                config_with_profile("reviewer")
            ),
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();

        assert!(config.review.enabled);
        assert_eq!(
            config.review.tier,
            crate::hel_review::lanes::ReviewTier::Extended
        );
        assert_eq!(config.review.reviewer_profile(), Some("reviewer"));
        assert_eq!(config.review.model.as_deref(), Some("opus"));
        assert_eq!(config.review.effort, None);
    }

    /// Arming review without naming a reviewer has no sensible default: Mjolnir
    /// will not choose which agent reviews on the user's behalf.
    #[test]
    fn arming_review_without_a_profile_is_refused() {
        let config = HelConfig {
            review: ReviewConfig {
                enabled: true,
                ..ReviewConfig::default()
            },
            ..HelConfig::default()
        };
        let error = config
            .validate()
            .expect_err("armed review needs a reviewer");
        assert!(
            format!("{error:#}").contains("needs `profile`"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn a_review_profile_that_names_nothing_is_refused() {
        let config = HelConfig {
            review: ReviewConfig {
                profile: Some("missing".into()),
                ..ReviewConfig::default()
            },
            ..HelConfig::default()
        };
        let error = config
            .validate()
            .expect_err("a reviewer must be a profile in this file");
        assert!(
            format!("{error:#}").contains("not a profile defined in this config"),
            "unexpected error: {error:#}"
        );
    }

    /// A one-off `/review` needs a reviewer without automatic review, so a
    /// profile with `enabled = false` is a valid configuration.
    #[test]
    fn a_reviewer_without_automatic_review_is_valid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "{}\n[review]\nprofile = \"reviewer\"\n",
                config_with_profile("reviewer")
            ),
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();
        assert!(!config.review.enabled);
        assert_eq!(config.review.reviewer_profile(), Some("reviewer"));
    }

    /// A review section that survives salvage is one whose profile also
    /// survived: the section is only usable if its reviewer exists.
    #[test]
    fn salvage_keeps_a_review_section_whose_profile_survived() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "version = 9999\n{}\n[review]\nenabled = true\nprofile = \"reviewer\"\n",
                config_with_profile("reviewer")
                    .strip_prefix("version = 1\n")
                    .unwrap()
            ),
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.newer_config_version, Some(9999));
        assert!(config.review.enabled);
        assert_eq!(config.review.reviewer_profile(), Some("reviewer"));
    }

    #[test]
    fn salvage_drops_a_review_section_whose_profile_did_not_survive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "version = 9999\n[review]\nenabled = true\nprofile = \"gone\"\n",
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();
        assert_eq!(config.review, ReviewConfig::default());
    }

    #[test]
    fn explicit_web_viewer_opt_out_survives_serialization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = HelConfig::default();
        config.phone.enabled = false;
        config.phone.tailscale_detect = false;

        config.save_to(&path).unwrap();
        let body = fs::read_to_string(&path).unwrap();

        assert!(body.contains("enabled = false"), "{body}");
        assert!(body.contains("tailscale_detect = false"), "{body}");
        assert_eq!(HelConfig::load_from(&path).unwrap(), config);
    }

    #[test]
    fn phone_config_requires_tls_off_loopback_and_complete_key_pairs() {
        let mut config = HelConfig::default();
        config.phone.enabled = true;
        config.phone.bind = "0.0.0.0:3765".into();
        assert!(config.validate().unwrap_err().to_string().contains("TLS"));

        config.phone.tls_cert = Some(PathBuf::from("certificate.pem"));
        assert!(config.validate().unwrap_err().to_string().contains("both"));
        config.phone.tls_key = Some(PathBuf::from("private-key.pem"));
        config.validate().unwrap();
    }

    #[test]
    fn empty_config_uses_clean_v1_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "\n\t").unwrap();
        assert_eq!(HelConfig::load_from(&path).unwrap(), HelConfig::default());
    }

    #[test]
    fn newer_config_loads_read_only_instead_of_blocking_startup() {
        // Running a newer Mjolnir and then downgrading must not lock the user out
        // of the older build.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let body = format!(
            "version = {}\nsetting_from_the_future = true\n\n[targets.localhost]\nkind = \
             \"local-bare\"\n",
            CONFIG_VERSION + 1
        );
        fs::write(&path, &body).unwrap();

        let config = HelConfig::load_from(&path).unwrap();

        // The settings the newer build saved still work.
        assert_eq!(
            config.targets.get("localhost"),
            Some(&TargetTemplate::LocalBare)
        );
        assert_eq!(config.newer_config_version, Some(CONFIG_VERSION + 1));
        assert!(
            config
                .newer_build_notice()
                .is_some_and(|notice| notice.contains("newer Mjolnir"))
        );

        // Saving would downgrade the newer build's file, so it must refuse and
        // leave the file byte for byte as it was.
        let error = config.save_to(&path).unwrap_err().to_string();
        assert!(error.contains("newer Mjolnir"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn newer_config_keeps_the_sections_this_build_still_understands() {
        // A future release reshapes one target and adds a section. Only the
        // reshaped target is lost; everything else still loads.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "version = {}\n\n[future_section]\nwhatever = 1\n\n[profiles.codex-1]\nkind \
                 = \"codex\"\nhome = \"/home/test/.codex-one\"\n\n[targets.localhost]\nkind \
                 = \"local-bare\"\n\n[targets.future]\nkind = \"quantum-sandbox\"\n",
                CONFIG_VERSION + 1
            ),
        )
        .unwrap();

        let config = HelConfig::load_from(&path).unwrap();

        assert!(config.profiles.contains_key("codex-1"));
        assert_eq!(
            config.targets.get("localhost"),
            Some(&TargetTemplate::LocalBare)
        );
        assert!(!config.targets.contains_key("future"));
        assert_eq!(config.newer_config_version, Some(CONFIG_VERSION + 1));
    }

    #[test]
    fn a_newer_config_written_after_load_still_blocks_a_save() {
        // Another Hel may upgrade the file between this build's load and its
        // save; the save must re-check the file rather than trust its marker.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config = sample_config();
        config.save_to(&path).unwrap();

        let body = format!("version = {}\n", CONFIG_VERSION + 1);
        fs::write(&path, &body).unwrap();

        let error = config.save_to(&path).unwrap_err().to_string();
        assert!(error.contains("newer Mjolnir"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn the_legacy_localhost_rename_leaves_a_newer_config_alone() {
        // The rename runs at daemon startup and used to be a save; against a
        // read-only config it must skip instead of failing startup.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let body = format!(
            "version = {}\n\n[targets.raw-localhost]\nkind = \"local-bare\"\n",
            CONFIG_VERSION + 1
        );
        fs::write(&path, &body).unwrap();

        assert!(!HelConfig::migrate_legacy_localhost_target_at(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn an_older_config_version_is_still_rejected() {
        // Hel has no downgrade migration, so an unrecognized older schema
        // keeps reporting an error rather than guessing.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "version = 0\n").unwrap();

        let error = HelConfig::load_from(&path).unwrap_err().to_string();
        assert!(
            error.contains("unsupported Mjolnir config version 0"),
            "{error}"
        );
    }

    #[test]
    fn a_malformed_newer_config_is_still_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(&path, "version = 2\nthis is not toml\n").unwrap();

        let error = HelConfig::load_from(&path).unwrap_err().to_string();
        assert!(error.contains("parse Mjolnir config"), "{error}");
    }

    #[test]
    fn removed_profile_overrides_have_an_actionable_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "version = 1\n[profiles.codex]\nkind = \"codex\"\nhome = \"/tmp/codex\"\nmodel = \"gpt-old\"\n",
        )
        .unwrap();
        let error = HelConfig::load_from(&path).unwrap_err().to_string();
        assert!(error.contains("`model` is no longer supported"));
        assert!(error.contains("/config"));
    }

    #[test]
    fn profile_cannot_override_its_isolated_home() {
        let mut config = sample_config();
        config
            .profiles
            .get_mut("codex-1")
            .unwrap()
            .environment
            .insert("CODEX_HOME".into(), "/shared-and-racy".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("must use `home`")
        );
    }

    #[test]
    fn container_size_hosts_group_local_runtimes_and_exact_ssh_hosts() {
        let container = ContainerTemplate {
            image: "agent:latest".into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        };
        let podman = TargetTemplate::LocalPodman {
            container: container.clone(),
        };
        let apple = TargetTemplate::AppleContainer {
            container: container.clone(),
        };
        let ssh = TargetTemplate::SshPodman {
            ssh: SshConnection {
                host: "builder.example.test".into(),
                user: Some("dev".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            container,
        };

        assert_eq!(container_size_host(&podman), Some("local"));
        assert_eq!(container_size_host(&apple), Some("local"));
        assert_eq!(container_size_host(&ssh), Some("builder.example.test"));
        assert_eq!(container_size_host(&TargetTemplate::LocalBare), None);
    }
    #[test]
    fn ssh_docker_target_round_trips_and_rejects_podman_storage() {
        let text = r#"kind = "ssh-docker"
host = "builder"
user = "ubuntu"
image = "ubuntu:24.04"
"#;
        let target: TargetTemplate = toml::from_str(text).unwrap();
        target.validate("remote-docker").unwrap();
        assert_eq!(
            toml::from_str::<TargetTemplate>(&toml::to_string(&target).unwrap()).unwrap(),
            target
        );
        assert_eq!(container_size_host(&target), Some("builder"));
        let TargetTemplate::SshDocker { ssh, mut container } = target else {
            panic!("wrong kind")
        };
        container.workspace_storage = PodmanWorkspaceStorage::ContainerLayer;
        assert!(
            TargetTemplate::SshDocker { ssh, container }
                .validate("remote-docker")
                .unwrap_err()
                .to_string()
                .contains("only supported by Podman")
        );
    }
}

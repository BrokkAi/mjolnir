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

use crate::codex_provider::CodexProvider;
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
    pub tier: crate::review::lanes::ReviewTier,
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

fn is_default_tier(tier: &crate::review::lanes::ReviewTier) -> bool {
    *tier == crate::review::lanes::ReviewTier::default()
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

fn default_subagent_limit() -> usize {
    6
}

fn is_default_subagent_limit(value: &usize) -> bool {
    *value == default_subagent_limit()
}

/// Global policy for Mjolnir-managed child agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(
        default = "default_subagent_limit",
        skip_serializing_if = "is_default_subagent_limit"
    )]
    pub max_concurrent: usize,
    /// Profiles available in addition to the parent's own enabled profile.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub eligible_profiles: BTreeMap<String, bool>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: default_subagent_limit(),
            eligible_profiles: BTreeMap::new(),
        }
    }
}

impl SubagentConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn validate(&self, profiles: &BTreeMap<String, HarnessProfile>) -> Result<()> {
        if !(1..=64).contains(&self.max_concurrent) {
            bail!("[subagents] `max_concurrent` must be between 1 and 64");
        }
        for profile_id in self
            .eligible_profiles
            .iter()
            .filter_map(|(profile_id, eligible)| eligible.then_some(profile_id))
        {
            validate_id("sub-agent profile", profile_id)?;
            match profiles.get(profile_id) {
                Some(profile) if profile.enabled => {}
                Some(_) => bail!("[subagents] eligible profile {profile_id:?} is disabled"),
                None => bail!(
                    "[subagents] eligible profile {profile_id:?} is not defined in this config"
                ),
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn profile_is_eligible(&self, parent: &str, candidate: &str) -> bool {
        self.enabled
            && (parent == candidate
                || self
                    .eligible_profiles
                    .get(candidate)
                    .copied()
                    .unwrap_or(false))
    }
}

pub const CONFIG_VERSION: u32 = 9;
pub const PRODUCT_DIR: &str = "mjolnir";
pub const DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/brokkai/mjolnir/agent-dev:latest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    Codex,
    Claude,
    Kimi,
    Grok,
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
    /// A word appended once to a whitespace-separated argv held in an env var.
    launch_argument: Option<(&'static str, &'static str)>,
    /// Value for the ACP `session/new` `_meta.sandbox.enabled` field.
    session_sandbox: Option<bool>,
    /// A value written into a staged profile file before upload.
    staged_setting: Option<StagedSetting>,
}

/// A setting the controller writes into a staged harness profile because the
/// harness reads it from disk and no launch-time channel can carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedSetting {
    /// File inside the staged profile, for example `settings.json`.
    pub file: &'static str,
    /// Object keys walked from the document root to the value's key.
    pub path: &'static [&'static str],
    pub value: &'static str,
    /// Key inserted when absent on the root and on every object created or
    /// traversed along `path`. Muse requires `schema_version: 1` on both.
    pub object_version: Option<(&'static str, u64)>,
}

impl StagedSetting {
    /// Write this setting into a staged document's root object, creating the
    /// objects along `path` and stamping `object_version` where it is missing.
    pub fn apply(&self, root: &mut serde_json::Map<String, serde_json::Value>) -> Result<()> {
        let (key, parents) = self
            .path
            .split_last()
            .context("staged setting path must name a key")?;
        let mut object = root;
        for parent in parents {
            self.stamp_version(object);
            object = object
                .entry(*parent)
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .with_context(|| format!("{parent} must be a JSON object"))?;
        }
        self.stamp_version(object);
        object.insert((*key).to_owned(), serde_json::Value::from(self.value));
        Ok(())
    }

    fn stamp_version(&self, object: &mut serde_json::Map<String, serde_json::Value>) {
        if let Some((key, version)) = self.object_version {
            object
                .entry(key)
                .or_insert_with(|| serde_json::Value::from(version));
        }
    }
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

    /// An argv word appended to the environment variable that carries the
    /// harness's own command line, when the policy needs one.
    pub const fn launch_argument(self) -> Option<(&'static str, &'static str)> {
        self.launch_argument
    }

    /// What the ACP `session/new` request asks for the harness's own sandbox.
    pub const fn session_sandbox(self) -> Option<bool> {
        self.session_sandbox
    }

    /// A setting the controller writes into the staged profile before upload.
    pub const fn staged_setting(self) -> Option<StagedSetting> {
        self.staged_setting
    }
}

/// The file inside a harness home that proves the harness is logged in.
///
/// Setup, quota checks, credential sync, and the target-side worker all read
/// the same path, so it is decided here beside [`HarnessKind`] rather than in
/// any one of them.
pub fn harness_authentication_marker(kind: HarnessKind, home: &Path) -> PathBuf {
    home.join(kind.credential_file_name())
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

    pub const ALL: [Self; 5] = [
        Self::Codex,
        Self::Claude,
        Self::Kimi,
        Self::Grok,
        Self::Muse,
    ];

    /// Environment variable used to isolate this harness's configuration.
    pub const fn home_env(self) -> &'static str {
        match self {
            Self::Codex => "CODEX_HOME",
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Kimi => "KIMI_CODE_HOME",
            Self::Grok => "GROK_HOME",
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
            Self::Muse => ".config/muse",
        }
    }

    /// The harness-home-relative file that proves the harness is logged in.
    /// Join it onto a home with [`harness_authentication_marker`].
    pub const fn credential_file_name(self) -> &'static str {
        match self {
            Self::Codex => "auth.json",
            Self::Claude => ".credentials.json",
            Self::Kimi => "credentials/kimi-code.json",
            Self::Grok => "auth.json",
            Self::Muse => "auth.json",
        }
    }

    /// The harness's own command-line program, as found on `PATH`.
    pub const fn cli_binary_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Muse => "muse",
        }
    }

    /// The project instruction file this harness reads.
    pub const fn agent_instructions_file(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE.md",
            Self::Codex | Self::Kimi | Self::Grok | Self::Muse => "AGENTS.md",
        }
    }

    /// The harness-home-relative directories Hel keeps in sync for a profile.
    ///
    /// Every harness resolves user skills from a `skills/` directory under its
    /// home, matching the provisioning allowlist the controller stages.
    pub const fn synced_skill_dirs(self) -> &'static [&'static str] {
        match self {
            Self::Codex | Self::Claude | Self::Kimi | Self::Grok | Self::Muse => &["skills"],
        }
    }

    /// Lowercase stable identifier used in config, storage, and the HTTP API.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
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
            Self::Muse => "Muse Code",
        }
    }

    /// How this harness realizes a target-level execution policy. Configured
    /// approvals preserve harness configuration, except that Codex and Claude
    /// select their guardian mode explicitly.
    pub const fn execution_enforcement(
        self,
        policy: ExecutionPolicy,
    ) -> Option<ExecutionEnforcement> {
        match (self, policy) {
            (Self::Muse, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "allowAll / sandbox-off / :unrestricted",
                acp_mode: Some("allowAll"),
                launch_flag: None,
                launch_environment: Some(("MUSE_APPROVAL_MODE", "allowAll")),
                launch_argument: Some(("MUSE_SERVE_ARGS", "--disable-sandbox")),
                session_sandbox: None,
                staged_setting: Some(StagedSetting {
                    file: "settings.json",
                    path: &["permissions", "default_profile"],
                    value: ":unrestricted",
                    object_version: Some(("schema_version", 1)),
                }),
            }),
            (Self::Codex, ExecutionPolicy::ConfiguredApprovals) => Some(ExecutionEnforcement {
                label: "agent / guardian",
                acp_mode: Some("agent"),
                launch_flag: None,
                launch_environment: Some(("INITIAL_AGENT_MODE", "agent")),
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            (Self::Codex, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "agent-full-access",
                acp_mode: Some("agent-full-access"),
                launch_flag: None,
                launch_environment: Some(("INITIAL_AGENT_MODE", "agent-full-access")),
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            // Claude's guardian is its Auto mode: Claude decides each
            // permission itself instead of asking the client, which has no
            // per-tool approval surface of its own.
            (Self::Claude, ExecutionPolicy::ConfiguredApprovals) => Some(ExecutionEnforcement {
                label: "auto / guardian",
                acp_mode: Some("auto"),
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            // Every remaining harness keeps the configuration its user wrote.
            // Muse never reaches this arm: `effective_execution_policy` has
            // already forced it unconstrained.
            (_, ExecutionPolicy::ConfiguredApprovals) => None,
            (Self::Claude, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "bypassPermissions / sandbox-off",
                acp_mode: Some("bypassPermissions"),
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: Some(false),
                staged_setting: None,
            }),
            (Self::Kimi, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "auto",
                acp_mode: Some("auto"),
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            (Self::Grok, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "always-approve / sandbox-off",
                acp_mode: None,
                launch_flag: Some("--always-approve"),
                launch_environment: Some(("GROK_SANDBOX", "off")),
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
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
        let Some(enforcement) = self.execution_enforcement(policy) else {
            return Ok(());
        };
        if let Some((key, argument)) = enforcement.launch_argument() {
            let args = environment.entry(key.to_owned()).or_default();
            if !args.split_whitespace().any(|word| word == argument) {
                if !args.is_empty() {
                    args.push(' ');
                }
                args.push_str(argument);
            }
        }
        if let Some((key, value)) = enforcement.launch_environment() {
            environment.insert(key.to_owned(), value.to_owned());
        }
        Ok(())
    }

    pub const fn supports_guardian_approvals(self) -> bool {
        matches!(self, Self::Codex | Self::Claude | Self::Grok)
    }

    /// The policy a session actually runs under. Muse cannot honor configured
    /// approvals: its permission profile is a host-lifetime setting that
    /// `muse serve` refuses when it names the automated reviewer, and the wire
    /// cannot select another. Muse therefore runs unconstrained on every
    /// target and the target wizard warns on raw ones.
    pub const fn effective_execution_policy(self, target: ExecutionPolicy) -> ExecutionPolicy {
        match self {
            Self::Muse => ExecutionPolicy::Unconstrained,
            _ => target,
        }
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
    /// Which model reviews this profile's escalated actions in Codex's Guardian
    /// mode: `"newest-flash"` (the default when absent) picks the newest flash
    /// model the provider's catalog lists, `"session"` leaves reviews to the
    /// session's own model, and any other value names a catalog slug. Only
    /// meaningful for a Codex profile with a custom model provider, because
    /// Mjolnir generates the catalog only for those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_review_model: Option<String>,
}

/// The `guardian_review_model` value that picks the newest flash model.
pub const GUARDIAN_REVIEW_NEWEST_FLASH: &str = "newest-flash";
/// The `guardian_review_model` value that leaves reviews to the session model.
pub const GUARDIAN_REVIEW_SESSION: &str = "session";

impl HarnessProfile {
    /// Discovery identifies an installation independently of user settings.
    pub fn same_installation(&self, other: &Self) -> bool {
        self.kind == other.kind && self.home == other.home
    }

    pub fn home_env(&self) -> &'static str {
        self.kind.home_env()
    }

    pub fn execution_enforcement(&self, policy: ExecutionPolicy) -> Option<ExecutionEnforcement> {
        self.kind.execution_enforcement(policy)
    }

    /// The custom model provider named in this profile's Codex `config.toml`,
    /// if any. Always `None` for a harness other than Codex, and for a Codex
    /// profile whose home does not exist yet or uses Codex's own provider.
    pub fn codex_provider(&self) -> Result<Option<CodexProvider>> {
        if self.kind != HarnessKind::Codex {
            return Ok(None);
        }
        crate::codex_provider::codex_provider(&self.home)
    }

    /// How this profile proves it may talk to its service.
    ///
    /// `ApiKey` means the key is supplied through the named environment
    /// variable from the profile's `environment` map, so there is no
    /// interactive login and no credential file to sync or expire. Every other
    /// profile, including a Codex provider that inlines its key as
    /// `experimental_bearer_token`, reports `NativeLogin`: for the inline form
    /// the key already sits inside the staged configuration file, which is the
    /// same file the authentication gate checks. Prefer `env_key` so the key
    /// never lands in a staged file.
    pub fn auth_scheme(&self) -> AuthScheme {
        match self.codex_provider() {
            Ok(Some(provider)) => match provider.env_key {
                Some(env_key) => AuthScheme::ApiKey { env_key },
                None => AuthScheme::NativeLogin,
            },
            // An unreadable or malformed home is reported where it is read
            // (validation, staging, doctor), not silently here.
            Ok(None) | Err(_) => AuthScheme::NativeLogin,
        }
    }

    /// The file inside this profile's home that proves it is authenticated.
    /// An API-key profile is proven by its Codex `config.toml`, because the key
    /// itself lives in the profile environment rather than in a file.
    pub fn authentication_marker(&self) -> PathBuf {
        match self.auth_scheme() {
            AuthScheme::ApiKey { .. } => self.home.join("config.toml"),
            AuthScheme::NativeLogin => harness_authentication_marker(self.kind, &self.home),
        }
    }

    /// See [`crate::credentials::credential_freshness`]. An API key does not
    /// expire or refresh, so it orders no copies.
    pub fn credential_freshness(&self, bytes: &[u8]) -> Option<i64> {
        match self.auth_scheme() {
            AuthScheme::ApiKey { .. } => None,
            AuthScheme::NativeLogin => crate::credentials::credential_freshness(self.kind, bytes),
        }
    }

    /// See [`crate::credentials::credential_expiry`]. An API key has no expiry.
    pub fn credential_expiry(&self, bytes: &[u8]) -> Option<i64> {
        match self.auth_scheme() {
            AuthScheme::ApiKey { .. } => None,
            AuthScheme::NativeLogin => crate::credentials::credential_expiry(self.kind, bytes),
        }
    }

    /// Whether this profile can review actions that leave the sandbox before
    /// running them. Codex runs its Guardian reviewer against whatever provider
    /// the profile names, so the answer depends only on the harness kind.
    pub fn supports_guardian_approvals(&self) -> bool {
        self.kind.supports_guardian_approvals()
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
        let provider = self
            .codex_provider()
            .with_context(|| format!("profile {id:?}"))?;
        if let Some(provider) = provider.as_ref() {
            if provider.model_catalog_json.is_some() {
                bail!(
                    "profile {id:?}: remove `model_catalog_json` from {}; Mjolnir fetches the model catalog from {} and stages it for every launch",
                    self.home.join("config.toml").display(),
                    provider.base_url
                );
            }
            if let Some(env_key) = provider.env_key.as_deref()
                && self
                    .environment
                    .get(env_key)
                    .is_none_or(|value| value.trim().is_empty())
            {
                bail!(
                    "profile {id:?}: {} authenticates with {env_key}, so set it under [profiles.{id}.environment] in Mjolnir's config.toml",
                    self.home.join("config.toml").display()
                );
            }
        }
        if let Some(reviewer) = self.guardian_review_model.as_deref() {
            if reviewer.trim().is_empty() {
                bail!(
                    "profile {id:?}: `guardian_review_model` must be {GUARDIAN_REVIEW_NEWEST_FLASH:?}, {GUARDIAN_REVIEW_SESSION:?}, or a model slug from the provider's catalog"
                );
            }
            if provider.is_none() {
                bail!(
                    "profile {id:?}: `guardian_review_model` applies only to a Codex profile whose {} names a custom model provider, because Mjolnir generates the model catalog only for those",
                    self.home.join("config.toml").display()
                );
            }
        }
        Ok(())
    }
}

/// How a profile proves it may talk to its service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthScheme {
    /// The harness's own login writes a credential file into the profile home.
    NativeLogin,
    /// A long-lived API key supplied through this environment variable.
    ApiKey { env_key: String },
}

impl AuthScheme {
    pub const fn is_api_key(&self) -> bool {
        matches!(self, Self::ApiKey { .. })
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
    /// The `kind` spelling used in configuration and on the wire.
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::LocalBare => "local-bare",
            Self::LocalPodman { .. } => "local-podman",
            Self::LocalDocker { .. } => "local-docker",
            Self::AppleContainer { .. } => "apple-container",
            Self::AwsEc2 { .. } => "aws-ec2",
            Self::SshBare { .. } => "ssh-bare",
            Self::SshPodman { .. } => "ssh-podman",
            Self::SshDocker { .. } => "ssh-docker",
        }
    }

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

// Old config files may still contain [startup]. Accept it without retaining
// settings that could recreate automatic first-session behavior on save.
fn discard_legacy_startup<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<(), D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| ())
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
pub struct Config {
    /// Deprecated session filtering preference retained for read compatibility.
    /// It is ignored and omitted from newly written configurations.
    #[serde(default, skip_serializing)]
    pub show_stopped_sessions: bool,
    #[serde(default, skip_serializing_if = "SessionsSide::is_default")]
    pub sessions_side: SessionsSide,
    #[serde(default, skip_serializing_if = "AdvancedConfig::is_default")]
    pub advanced: AdvancedConfig,
    pub version: u32,
    /// Client-side activity animation; omitted configurations retain the classic scan.
    #[serde(default, skip_serializing_if = "SpinnerStyle::is_default")]
    pub spinner: SpinnerStyle,
    #[serde(default, skip_serializing_if = "UiTheme::is_default")]
    pub theme: UiTheme,
    #[serde(default, skip_serializing_if = "PhoneConfig::is_default")]
    pub phone: PhoneConfig,
    #[serde(default, skip_serializing_if = "ReviewConfig::is_default")]
    pub review: ReviewConfig,
    #[serde(default, skip_serializing_if = "SubagentConfig::is_default")]
    pub subagents: SubagentConfig,
    #[serde(
        default,
        rename = "startup",
        skip_serializing,
        deserialize_with = "discard_legacy_startup"
    )]
    pub legacy_startup: (),
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, HarnessProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bundles: BTreeMap<String, ProjectBundle>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, TargetTemplate>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sessions_side: SessionsSide::default(),
            advanced: AdvancedConfig::default(),
            show_stopped_sessions: false,
            version: CONFIG_VERSION,
            spinner: SpinnerStyle::default(),
            theme: Default::default(),
            phone: PhoneConfig::default(),
            review: ReviewConfig::default(),
            subagents: SubagentConfig::default(),
            legacy_startup: (),
            profiles: BTreeMap::new(),
            bundles: BTreeMap::new(),
            targets: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Suggest additions for setup without replacing existing entries. Both
    /// setup surfaces use the same identities and collision-free names.
    pub fn setup_additions(&self, discovered: &Self) -> Self {
        fn additions<T: Clone>(
            existing: &BTreeMap<String, T>,
            discovered: &BTreeMap<String, T>,
            same: impl Fn(&T, &T) -> bool,
            base: impl Fn(&str, &T) -> String,
        ) -> BTreeMap<String, T> {
            let mut known = existing.clone();
            let mut added = BTreeMap::new();
            for (id, value) in discovered {
                if known.values().any(|entry| same(entry, value)) {
                    continue;
                }
                let id = unique_config_id(&known, &base(id, value));
                known.insert(id.clone(), value.clone());
                added.insert(id, value.clone());
            }
            added
        }
        Self {
            profiles: additions(
                &self.profiles,
                &discovered.profiles,
                HarnessProfile::same_installation,
                |id, _| id.to_owned(),
            ),
            bundles: additions(
                &self.bundles,
                &discovered.bundles,
                PartialEq::eq,
                |_, bundle| bundle.primary_repo.clone(),
            ),
            targets: additions(
                &self.targets,
                &discovered.targets,
                PartialEq::eq,
                |id, _| id.to_owned(),
            ),
            ..Self::default()
        }
    }

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
        self.subagents.validate(&self.profiles)?;
        for (id, bundle) in &self.bundles {
            bundle.validate(id)?;
        }
        for (id, target) in &self.targets {
            target.validate(id)?;
        }
        Ok(())
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()).map(Self::with_local_targets)
    }

    /// Supply standard local choices without requiring setup or writing a file.
    /// These are candidates: callers must check availability before offering launch.
    /// Explicit entries with the same name override the standard defaults.
    pub fn with_local_targets(mut self) -> Self {
        let container = ContainerTemplate {
            image: DEFAULT_CONTAINER_IMAGE.into(),
            pull_policy: Default::default(),
            platform: None,
            cpus: None,
            memory: None,
            environment: BTreeMap::new(),
            workspace_storage: Default::default(),
        };
        #[cfg(unix)]
        self.targets
            .entry("localhost".into())
            .or_insert(TargetTemplate::LocalBare);
        self.targets
            .entry("podman".into())
            .or_insert_with(|| TargetTemplate::LocalPodman {
                container: container.clone(),
            });
        self.targets
            .entry("docker".into())
            .or_insert_with(|| TargetTemplate::LocalDocker {
                container: container.clone(),
            });
        #[cfg(target_os = "macos")]
        self.targets
            .entry("apple-container".into())
            .or_insert_with(|| TargetTemplate::AppleContainer { container });
        self
    }

    /// Read the config from `path`, returning [`Config::default`] when the
    /// file is missing or empty and an error when it is malformed or was
    /// written by a newer Mjolnir.
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
            bail!(
                "{} was written by a newer Mjolnir (config version {found}; this build supports \
                 {CONFIG_VERSION}). Update Mjolnir",
                path.display()
            );
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
        // disabled; version 9 adds sub-agent policy. Earlier configs acquire
        // defaults in memory and upgrade on
        // the next ordinary save.
        if matches!(config.version, 1..=8) {
            config.version = CONFIG_VERSION;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Load the latest config, apply one edit, validate it, and save it while
    /// holding the config lock for the complete transaction.
    ///
    /// The returned config includes runtime local defaults, and the second
    /// value is whatever the edit returned. Keeping the load and edit under
    /// the same lock is what lets independent processes update disjoint
    /// sections without one stale full-config save erasing the other.
    pub fn update<T, F>(edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        Self::update_to(&config_path(), |config| {
            // Edits see the same local candidates as reads, but unrelated
            // changes must not write implicit defaults into the user's file.
            let mut runtime = config.clone().with_local_targets();
            let implicit: Vec<_> = runtime
                .targets
                .iter()
                .filter(|(id, _)| !config.targets.contains_key(*id))
                .map(|(id, target)| (id.clone(), target.clone()))
                .collect();
            let value = edit(&mut runtime)?;
            for (id, target) in implicit {
                if runtime.targets.get(&id) == Some(&target) {
                    runtime.targets.remove(&id);
                }
            }
            *config = runtime;
            Ok(value)
        })
        .map(|(config, value)| (config.with_local_targets(), value))
    }

    /// As [`Self::update`], using an explicit config path.
    pub fn update_to<T, F>(path: &Path, edit: F) -> Result<(Self, T)>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        let _lock = ConfigLock::acquire(path)?;
        let mut config = Self::load_from(path)?;
        let value = edit(&mut config)?;
        config.save_to_locked(path)?;
        Ok((config, value))
    }

    /// Loads the current file, replaces only its global review section, and
    /// writes it atomically. Callers use this for the dashboard editor so a
    /// stale dashboard snapshot cannot overwrite profiles, bundles, targets,
    /// or phone settings changed concurrently by another client.
    pub fn save_review(review: ReviewConfig) -> Result<Self> {
        Self::save_review_to(&config_path(), review).map(Self::with_local_targets)
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
        Self::ensure_writable(path)?;
        self.validate()?;
        let body = toml::to_string_pretty(self).context("serialize Mjolnir config")?;
        atomic_write(path, body.as_bytes())
    }

    /// Refuse to overwrite a file that a newer Mjolnir wrote after this
    /// config was loaded.
    fn ensure_writable(path: &Path) -> Result<()> {
        if let Some(found) = newer_version_on_disk(path) {
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
pub(crate) struct ConfigLock {
    _file: File,
}

impl ConfigLock {
    pub(crate) fn acquire(config_path: &Path) -> Result<Self> {
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
pub fn newer_version_on_disk(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    newer_version(&contents.parse::<toml::Value>().ok()?)
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

/// Environment variable selecting an isolated Mjolnir instance. An instance
/// keeps its own configuration, database, daemon, and logs, so `MJ_INSTANCE=dev`
/// never shares state with the default setup.
pub const INSTANCE_ENV: &str = "MJ_INSTANCE";

/// Directory under the default configuration and data roots holding one
/// isolated instance's files (`<root>/mjolnir/instances/<name>`).
const INSTANCE_DIR: &str = "instances";

/// Instance name from [`INSTANCE_ENV`], trimmed. Empty means no instance.
pub fn instance_name() -> Option<String> {
    let name = env_override("INSTANCE")?;
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Whether `name` is safe to use as a single path segment under [`INSTANCE_DIR`].
pub fn is_valid_instance_name(name: &str) -> bool {
    validate_id("instance", name).is_ok()
}

/// Fail when [`INSTANCE_ENV`] names something that cannot be an instance.
/// Every shipped binary calls this during startup so a typo fails closed
/// instead of silently using the default directories.
pub fn validate_instance_env() -> Result<()> {
    if let Some(name) = instance_name() {
        validate_id("instance", &name)?;
    }
    Ok(())
}

/// Record a `--instance` flag value for this process and the daemon and
/// workers it spawns. The explicit flag wins over [`INSTANCE_ENV`].
pub fn apply_instance_flag(value: Option<&str>) -> Result<()> {
    if let Some(raw) = value {
        let name = raw.trim();
        validate_id("instance", name)?;
        // SAFETY: every caller runs this during single-threaded process startup,
        // before the Tokio runtime or any other thread exists, so no other
        // thread can observe the environment while it is being mutated.
        unsafe {
            std::env::set_var(INSTANCE_ENV, name);
        }
    }
    validate_instance_env()
}

/// Nest `base` under [`INSTANCE_DIR`] when an instance is selected. A name that
/// fails validation falls back to `base`; startup validation rejects it first,
/// so this only guards against future callers that skip that check.
fn with_instance_dir(base: PathBuf, instance: Option<&str>) -> PathBuf {
    match instance {
        Some(name) if is_valid_instance_name(name) => base.join(INSTANCE_DIR).join(name),
        _ => base,
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(path) = env_override_os("CONFIG_DIR") {
        return PathBuf::from(path);
    }
    with_instance_dir(
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join(PRODUCT_DIR),
        instance_name().as_deref(),
    )
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    if let Some(path) = env_override_os("DATA_DIR") {
        return PathBuf::from(path);
    }
    with_instance_dir(
        dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join(PRODUCT_DIR),
        instance_name().as_deref(),
    )
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// The first available configuration identifier, retaining a readable base.
pub fn unique_config_id<T>(entries: &BTreeMap<String, T>, base: &str) -> String {
    if !entries.contains_key(base) {
        return base.to_owned();
    }
    for number in 2.. {
        let suffix = format!("-{number}");
        // Configuration identifiers are ASCII and limited to 64 bytes.
        let prefix = base.chars().take(64 - suffix.len()).collect::<String>();
        let candidate = format!("{prefix}{suffix}");
        if !entries.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("configuration identifier space exhausted")
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

/// Make a rename or new entry in `path` durable. Directory fsync is only
/// available on Unix; Windows cannot open a directory handle this way.
pub fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests;

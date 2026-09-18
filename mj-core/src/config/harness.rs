//! Harness identity, credentials layout and execution policy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{default_true, is_true, validate_id};
use crate::codex_provider::CodexProvider;

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

    /// The file in a staged harness home that holds its MCP server list, for
    /// the harnesses that read that list from disk rather than over ACP.
    pub const fn mcp_config_file(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some(".claude.json"),
            Self::Kimi => Some("mcp.json"),
            Self::Codex | Self::Grok | Self::Muse => None,
        }
    }

    /// Whether this harness receives Mjolnir's delegation tools, and so gets
    /// the subagent choice in the wizards.
    pub const fn supports_delegation_tools(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }

    /// The harness-home-relative directories that hold its native session
    /// files, scanned when a checkpoint captures or restores native state.
    pub const fn native_session_dirs(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["sessions", "archived_sessions"],
            Self::Claude => &["projects", "session-env", "file-history"],
            Self::Kimi | Self::Grok => &["sessions"],
            Self::Muse => &[".data/muse/sessions"],
        }
    }

    /// An executable a managed install must create beside its pinned
    /// entrypoint, relative to the install root.
    pub const fn extra_managed_entrypoint(self) -> Option<&'static str> {
        match self {
            Self::Muse => Some("bin/muse"),
            Self::Grok => Some("bin/agent"),
            Self::Codex | Self::Claude | Self::Kimi => None,
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

/// The transcript budget a profile without an explicit `context_window_bytes`
/// runs under. It lives beside the setting so compaction and the settings
/// screen read one number.
pub const DEFAULT_CONTEXT_BYTES: usize = 256 * 1024;

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

    pub(super) fn validate(&self, id: &str) -> Result<()> {
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

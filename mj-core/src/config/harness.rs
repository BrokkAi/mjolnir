//! Harness identity, credentials layout and execution policy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{Environment, default_true, is_true, validate_id};
use crate::codex_provider::CodexProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    Codex,
    Claude,
    Kimi,
    Grok,
    Muse,
    OpenCode,
}

/// The operating system of the machine a harness's own CLI runs on, as far as
/// where that harness keeps its login is concerned. See
/// [`HarnessKind::keeps_login_in_home`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessHost {
    MacOs,
    Other,
}

impl HarnessHost {
    /// The machine this process runs on.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }
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
    /// The config option that carries `acp_mode` when the harness keeps it
    /// apart from its Mode selector.
    acp_mode_selector: Option<&'static str>,
    /// A further ACP config option and value the policy requires.
    acp_setting: Option<(&'static str, &'static str)>,
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

    /// The config option to select `acp_mode` on, when the bridge offers it.
    /// Without it, the mode goes to the bridge's Mode selector.
    pub const fn acp_mode_selector(self) -> Option<&'static str> {
        self.acp_mode_selector
    }

    /// A further config option and value to select after the mode, when the
    /// policy needs one.
    pub const fn acp_setting(self) -> Option<(&'static str, &'static str)> {
        self.acp_setting
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
    /// Point this harness at `home` through its home variable. Muse and
    /// OpenCode resolve their configuration from `$XDG_CONFIG_HOME/<name>`, so
    /// both are given the parent of their home, with session data under
    /// `.data` beside it.
    ///
    /// Every session and every configuration probe runs from a home Mjolnir
    /// staged for it, on every target and every operating system, so this
    /// always sets the variable.
    pub fn configure_home_environment(
        self,
        home: &Path,
        environment: &mut BTreeMap<String, String>,
    ) {
        let config_root = if self.nested_home() {
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

    /// Point this harness's own CLI at a profile's configured home, for the
    /// commands that act on the person's own login rather than on a session:
    /// `mj login`, `claude setup-token`, and quota probes.
    ///
    /// `host` is the machine that runs the command. Where the login does not
    /// live in the home ([`HarnessKind::keeps_login_in_home`]), the variable is
    /// left unset, so the command finds the login and the settings the person
    /// signed in with. Setting it there could only move the command off those
    /// settings.
    pub fn configure_profile_home_environment(
        self,
        home: &Path,
        host: HarnessHost,
        environment: &mut BTreeMap<String, String>,
    ) {
        if self.keeps_login_in_home(host) {
            self.configure_home_environment(home, environment);
        }
    }

    /// Whether this harness keeps its login inside its home on `host`, so that
    /// pointing [`HarnessKind::home_env`] at a home selects that home's login.
    ///
    /// Claude Code on macOS keeps every profile's OAuth credentials in one
    /// Keychain item, `Claude Code-credentials`, whatever `CLAUDE_CONFIG_DIR`
    /// says (anthropics/claude-code#20553). A Claude home there holds
    /// settings, history and MCP servers, but not the login. A session still
    /// runs from its own staged home there; only the commands that act on the
    /// person's own home leave the variable unset
    /// ([`HarnessKind::configure_profile_home_environment`]).
    pub const fn keeps_login_in_home(self, host: HarnessHost) -> bool {
        !matches!((self, host), (Self::Claude, HarnessHost::MacOs))
    }

    /// Whether this harness's home is a named subdirectory of the directory
    /// its home variable points at. The XDG layout fixes the leaf name
    /// (`muse`, `opencode`), which is also [`HarnessKind::id`].
    pub const fn nested_home(self) -> bool {
        matches!(self, Self::Muse | Self::OpenCode)
    }

    pub fn home_from_environment(self, value: impl AsRef<Path>) -> PathBuf {
        if self.nested_home() {
            value.as_ref().join(self.id())
        } else {
            value.as_ref().to_path_buf()
        }
    }

    pub const ALL: [Self; 6] = [
        Self::Codex,
        Self::Claude,
        Self::Kimi,
        Self::Grok,
        Self::Muse,
        Self::OpenCode,
    ];

    /// Environment variable used to isolate this harness's configuration.
    pub const fn home_env(self) -> &'static str {
        match self {
            Self::Codex => "CODEX_HOME",
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Kimi => "KIMI_CODE_HOME",
            Self::Grok => "GROK_HOME",
            Self::Muse => "XDG_CONFIG_HOME",
            Self::OpenCode => "XDG_CONFIG_HOME",
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
            Self::OpenCode => ".config/opencode",
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
            Self::OpenCode => ".data/opencode/auth.json",
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
            Self::OpenCode => "opencode",
        }
    }

    /// The project instruction file this harness reads.
    pub const fn agent_instructions_file(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE.md",
            Self::Codex | Self::Kimi | Self::Grok | Self::Muse | Self::OpenCode => "AGENTS.md",
        }
    }

    /// The harness-home-relative directories Hel keeps in sync for a profile.
    ///
    /// Every harness resolves user skills from a `skills/` directory under its
    /// home, matching the provisioning allowlist the controller stages.
    pub const fn synced_skill_dirs(self) -> &'static [&'static str] {
        match self {
            Self::Codex | Self::Claude | Self::Kimi | Self::Grok | Self::Muse | Self::OpenCode => {
                &["skills"]
            }
        }
    }

    /// Home-relative paths inside a synced skills directory that the harness
    /// writes and maintains itself. Skills sync neither reads, copies, nor
    /// replaces them.
    ///
    /// Claude Code provisions `skills/synced/<org>_<user>/` from the user's
    /// claude.ai account and re-syncs it on its own; `skills/.trash/` is where
    /// it moves skills it removed. The Codex CLI writes its built-in skills
    /// into `skills/.system/`, with a `.codex-system-skills.marker` file there.
    ///
    /// Kimi, Grok, Muse and OpenCode keep none inside `skills/`. Kimi
    /// registers its built-in skills in memory, Grok caches its bundled skills
    /// under `bundled/skills/`, Muse writes its own under its data directory
    /// (`.data/muse/skills/` in a Mjolnir home), and OpenCode embeds its
    /// built-in skill.
    pub const fn harness_owned_skill_paths(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["skills/synced", "skills/.trash"],
            Self::Codex => &["skills/.system"],
            Self::Kimi | Self::Grok | Self::Muse | Self::OpenCode => &[],
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
            Self::OpenCode => "opencode",
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
            Self::OpenCode => "OpenCode",
        }
    }

    /// How to install this harness's own CLI, as the start of a sentence:
    /// "Install it with `npm install -g @openai/codex` (Node.js 22 or
    /// newer)". `mj login` and launch preflight say it when the CLI is
    /// missing.
    pub fn install_advice(self) -> String {
        match self {
            Self::Codex => {
                "Install it with `npm install -g @openai/codex` (Node.js 22 or newer)".to_owned()
            }
            Self::Claude => "Install it with `npm install -g @anthropic-ai/claude-code`".to_owned(),
            other => format!("Install the {} CLI", other.display_name()),
        }
    }

    /// Every harness by product name, in [`Self::ALL`] order, as a list that
    /// ends with "or": "Codex, Claude Code, Kimi Code, Grok Build, Muse Code,
    /// or OpenCode". Messages that say no agent was found use it, so each one
    /// names every agent Mjolnir looks for.
    pub fn every_display_name_or() -> String {
        let names = Self::ALL.map(Self::display_name);
        let (last, rest) = names.split_last().expect("ALL is not empty");
        format!("{}, or {last}", rest.join(", "))
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
                // muse-acp 0.10 puts Default, Read-only, and Plan on `mode`
                // and the approval policy on `approval_mode`. Container images
                // built before it keep the approval policy on `mode`.
                acp_mode_selector: Some("approval_mode"),
                acp_setting: None,
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
            // Muse's guardian is muse-acp's auto-review: a read-only Muse
            // reviewer answers each approval, and a failed review denies.
            // `muse serve` refuses Muse's own `:auto-review` profile, so the
            // staged profile asks and the adapter's reviewer answers.
            (Self::Muse, ExecutionPolicy::ConfiguredApprovals) => Some(ExecutionEnforcement {
                label: "promptUnmatched / auto-review / :ask-me",
                acp_mode: Some("promptUnmatched"),
                acp_mode_selector: Some("approval_mode"),
                acp_setting: Some(("auto_review", "on")),
                launch_flag: None,
                launch_environment: Some(("MUSE_APPROVAL_MODE", "promptUnmatched")),
                launch_argument: None,
                session_sandbox: None,
                staged_setting: Some(StagedSetting {
                    file: "settings.json",
                    path: &["permissions", "default_profile"],
                    value: ":ask-me",
                    object_version: Some(("schema_version", 1)),
                }),
            }),
            (Self::Codex, ExecutionPolicy::ConfiguredApprovals) => Some(ExecutionEnforcement {
                label: "agent / guardian",
                acp_mode: Some("agent"),
                acp_mode_selector: None,
                acp_setting: None,
                launch_flag: None,
                launch_environment: Some(("INITIAL_AGENT_MODE", "agent")),
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            (Self::Codex, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "agent-full-access",
                acp_mode: Some("agent-full-access"),
                acp_mode_selector: None,
                acp_setting: None,
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
                acp_mode_selector: None,
                acp_setting: None,
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            // Every remaining harness keeps the configuration its user wrote.
            (_, ExecutionPolicy::ConfiguredApprovals) => None,
            (Self::Claude, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "bypassPermissions / sandbox-off",
                acp_mode: Some("bypassPermissions"),
                acp_mode_selector: None,
                acp_setting: None,
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: Some(false),
                staged_setting: None,
            }),
            (Self::Kimi, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "auto",
                acp_mode: Some("auto"),
                acp_mode_selector: None,
                acp_setting: None,
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            (Self::Grok, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "always-approve / sandbox-off",
                acp_mode: None,
                acp_mode_selector: None,
                acp_setting: None,
                launch_flag: Some("--always-approve"),
                launch_environment: Some(("GROK_SANDBOX", "off")),
                launch_argument: None,
                session_sandbox: None,
                staged_setting: None,
            }),
            // OpenCode reads permission rules from its config file; `allow`
            // is its shorthand for allowing every tool without a prompt.
            (Self::OpenCode, ExecutionPolicy::Unconstrained) => Some(ExecutionEnforcement {
                label: "permission-allow",
                acp_mode: None,
                acp_mode_selector: None,
                acp_setting: None,
                launch_flag: None,
                launch_environment: None,
                launch_argument: None,
                session_sandbox: None,
                staged_setting: Some(StagedSetting {
                    file: "opencode.json",
                    path: &["permission"],
                    value: "allow",
                    object_version: None,
                }),
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
        matches!(
            self,
            Self::Codex | Self::Claude | Self::Grok | Self::OpenCode | Self::Muse
        )
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
            Self::Codex | Self::Grok | Self::Muse | Self::OpenCode => None,
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
            Self::OpenCode => &[".data/opencode"],
        }
    }

    /// Whether this harness's adapter passes on the MCP servers a session
    /// offers only when its `initialize` advertises HTTP MCP. muse-acp forwards
    /// them to Muse only under the host's session MCP grant, advertises HTTP
    /// MCP exactly then, and otherwise drops every server with a log line.
    pub const fn mcp_servers_need_advertised_http(self) -> bool {
        matches!(self, Self::Muse)
    }

    /// An executable a managed install must create beside its pinned
    /// entrypoint, relative to the install root.
    pub const fn extra_managed_entrypoint(self) -> Option<&'static str> {
        match self {
            Self::Muse => Some("bin/muse"),
            Self::Grok => Some("bin/agent"),
            Self::Claude => Some("node_modules/.bin/claude"),
            Self::Codex | Self::Kimi | Self::OpenCode => None,
        }
    }

    /// Harness-specific arguments that start its ACP stdio server.
    pub fn bridge_args(self, policy: ExecutionPolicy) -> Vec<&'static str> {
        let flag = self.launch_flag_for(policy);
        match self {
            Self::Codex | Self::Claude | Self::Muse => Vec::new(),
            Self::Kimi => vec!["acp"],
            Self::Grok => ["agent"].into_iter().chain(flag).chain(["stdio"]).collect(),
            Self::OpenCode => vec!["acp"],
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
    /// Passed to the harness and its bridge. An entry may name a secret
    /// instead of holding it; see [`super::Environment`].
    #[serde(default, skip_serializing_if = "Environment::is_empty")]
    pub environment: Environment,
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
    /// Creation default copied into each new session; existing sessions own their policy.
    #[serde(
        default,
        skip_serializing_if = "crate::subagent::SubagentPolicy::is_native"
    )]
    pub subagents: crate::subagent::SubagentPolicy,
}

/// Inputs that change advertised models and efforts. Session defaults and UI
/// preferences are deliberately absent. Use this projection in every cache.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProfileDiscoveryInputs {
    pub kind: HarnessKind,
    pub home: PathBuf,
    pub environment: BTreeMap<String, super::secrets::EnvironmentValue>,
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
    pub fn discovery_inputs(&self) -> ProfileDiscoveryInputs {
        ProfileDiscoveryInputs {
            kind: self.kind,
            home: self.home.clone(),
            environment: self.environment.sources().clone(),
        }
    }

    /// Stable identity of configured capabilities, independent of defaults,
    /// refreshed credentials and files the harness writes inside its home.
    pub fn capabilities_key(&self, id: &str) -> String {
        use sha2::{Digest, Sha256};
        let inputs = serde_json::to_vec(&(
            "profile-capabilities-v1",
            id,
            self.enabled,
            self.discovery_inputs(),
        ))
        .expect("profile capability inputs serialize");
        crate::hex::lower_hex(Sha256::digest(inputs))
    }

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

    /// The provider named in this profile's Codex `config.toml`, if any.
    /// Always `None` for another harness, or when Codex uses its default.
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
    /// interactive login and no credential file to sync or expire. Bedrock
    /// uses the AWS credential chain; local built-in providers need no login.
    /// A custom provider that inlines its key as `experimental_bearer_token`
    /// retains the existing `NativeLogin` behavior.
    pub fn auth_scheme(&self) -> AuthScheme {
        if self.kind == HarnessKind::Claude
            && self
                .environment
                .get("CLAUDE_CODE_USE_BEDROCK")
                .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE"))
        {
            return AuthScheme::AwsCredentialChain;
        }
        match self.codex_provider() {
            Ok(Some(provider)) if provider.uses_aws_credentials() => AuthScheme::AwsCredentialChain,
            Ok(Some(provider)) if provider.needs_no_authentication() => {
                AuthScheme::NoAuthentication
            }
            Ok(Some(provider)) => match provider.custom().and_then(|custom| custom.env_key.clone())
            {
                Some(env_key) => AuthScheme::ApiKey { env_key },
                None => AuthScheme::NativeLogin,
            },
            // An unreadable or malformed home is reported where it is read
            // (`ensure_ready`, staging, doctor), not silently here.
            Ok(None) | Err(_) => AuthScheme::NativeLogin,
        }
    }

    /// The file inside this profile's home that proves it is authenticated.
    /// An API-key profile is proven by its Codex `config.toml`, because the key
    /// itself lives in the profile environment rather than in a file.
    pub fn authentication_marker(&self) -> PathBuf {
        match self.auth_scheme() {
            AuthScheme::ApiKey { .. }
            | AuthScheme::AwsCredentialChain
            | AuthScheme::NoAuthentication => self.home.join("config.toml"),
            AuthScheme::NativeLogin => harness_authentication_marker(self.kind, &self.home),
        }
    }

    /// See [`crate::credentials::credential_freshness`]. An API key does not
    /// expire or refresh, so it orders no copies.
    pub fn credential_freshness(&self, bytes: &[u8]) -> Option<i64> {
        match self.auth_scheme() {
            AuthScheme::ApiKey { .. }
            | AuthScheme::AwsCredentialChain
            | AuthScheme::NoAuthentication => None,
            AuthScheme::NativeLogin => crate::credentials::credential_freshness(self.kind, bytes),
        }
    }

    /// See [`crate::credentials::credential_expiry`]. An API key has no expiry.
    pub fn credential_expiry(&self, bytes: &[u8]) -> Option<i64> {
        match self.auth_scheme() {
            AuthScheme::ApiKey { .. }
            | AuthScheme::AwsCredentialChain
            | AuthScheme::NoAuthentication => None,
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
        self.subagents
            .validate_profile(self.kind)
            .with_context(|| format!("profile {id:?}"))?;
        if self.kind.nested_home() {
            let leaf = self.kind.id();
            let name = self.kind.display_name();
            if self.home.file_name().is_none_or(|part| part != leaf) {
                bail!(
                    "{name} profile {id:?} home must end in /{leaf} (its XDG configuration directory)"
                );
            }
            if self.environment.contains_key("XDG_DATA_HOME") {
                bail!("{name} profile {id:?} must not override its managed XDG_DATA_HOME");
            }
        }
        if self.home.as_os_str().is_empty() {
            bail!("profile {id:?} has an empty home path");
        }
        // The entries as written, so one whose reference did not resolve is
        // checked too.
        let environment = self.environment.sources();
        if environment
            .keys()
            .any(|key| key.trim().is_empty() || key.contains('='))
        {
            bail!("profile {id:?} contains an invalid environment variable name");
        }
        if environment.contains_key(self.kind.home_env()) {
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
        if let Some(reviewer) = self.guardian_review_model.as_deref() {
            if reviewer.trim().is_empty() {
                bail!(
                    "profile {id:?}: `guardian_review_model` must be {GUARDIAN_REVIEW_NEWEST_FLASH:?}, {GUARDIAN_REVIEW_SESSION:?}, or a model slug from the provider's catalog"
                );
            }
            // Whether a Codex profile names a custom provider is up to its
            // home, which `ensure_ready` reads; another harness never does.
            if self.kind != HarnessKind::Codex {
                bail!(
                    "profile {id:?}: `guardian_review_model` applies only to a Codex profile whose config.toml names a custom model provider, because Mjolnir generates the model catalog only for those"
                );
            }
        }
        Ok(())
    }

    /// Supply the variable that this Codex home's custom provider names as its
    /// key (`env_key`) from the environment Mjolnir runs in, as standalone
    /// Codex reads it from its own, unless `environment` sets it. Workers
    /// start harnesses with a cleared environment, so without this an
    /// exported key never reaches Codex. Done once as the configuration is
    /// read, exactly when `{ from_env = ... }` entries resolve;
    /// [`Self::ensure_ready`] reports the key when neither supplies it.
    pub(super) fn inherit_provider_key(&mut self) {
        // A projection reads no files, and a malformed home is reported by
        // `ensure_ready`.
        if !super::secrets::resolves_values() {
            return;
        }
        if let Ok(Some(provider)) = self.codex_provider()
            && let Some(env_key) = provider
                .custom()
                .and_then(|custom| custom.env_key.as_deref())
        {
            self.environment.inherit(env_key);
        }
    }

    /// Whether a harness can start from this profile: every environment
    /// reference resolved, the harness home's own configuration readable, and
    /// the credential that configuration names supplied.
    ///
    /// These depend on files and variables outside Mjolnir's config.toml, so
    /// they are not part of [`Config::validate`](super::Config::validate): a
    /// profile that fails here is unusable, while the configuration, the
    /// daemon and every other profile keep working. Everything that starts a
    /// harness from a profile asks this first, and `mj doctor` reports it.
    ///
    /// The failure is a precondition refusal: the person fixes it, so it
    /// travels to the client as the reason rather than as an internal error.
    pub fn ensure_ready(&self, id: &str) -> Result<()> {
        let ready = || -> Result<()> {
            self.environment.ensure_resolved()?;
            let provider = self.codex_provider()?;
            if let Some(provider) = provider.as_ref()
                && let Some(env_key) = provider
                    .custom()
                    .and_then(|custom| custom.env_key.as_deref())
                && self
                    .environment
                    .get(env_key)
                    .is_none_or(|value| value.trim().is_empty())
            {
                bail!(
                    "{} authenticates with {env_key}, which is set neither in the environment Mjolnir started with nor under [profiles.{id}.environment]; export {env_key} and run `mj daemon restart`, or put it in secrets.toml and add `{env_key} = {{ from_secret = \"{env_key}\" }}` under [profiles.{id}.environment]",
                    self.home.join("config.toml").display()
                );
            }
            if self.guardian_review_model.is_some()
                && provider
                    .as_ref()
                    .is_none_or(|provider| provider.custom().is_none())
            {
                bail!(
                    "`guardian_review_model` applies only to a Codex profile whose {} names a custom model provider, because Mjolnir generates the model catalog only for those",
                    self.home.join("config.toml").display()
                );
            }
            Ok(())
        };
        ready().map_err(|error| {
            crate::refusal::Refusal::precondition(format!("profile {id:?}: {error:#}")).into()
        })
    }
}

/// How a profile proves it may talk to its service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthScheme {
    /// The harness's own login writes a credential file into the profile home.
    NativeLogin,
    /// A long-lived API key supplied through this environment variable.
    ApiKey { env_key: String },
    /// The harness obtains AWS credentials from its configured AWS provider chain.
    AwsCredentialChain,
    /// A built-in local provider does not require credentials.
    NoAuthentication,
}

impl AuthScheme {
    pub const fn is_api_key(&self) -> bool {
        matches!(self, Self::ApiKey { .. })
    }

    /// Whether this profile uses the harness's own login file.
    pub const fn uses_native_login_file(&self) -> bool {
        matches!(self, Self::NativeLogin)
    }
}

/// The variables through which Codex takes a credential other than its own
/// login file, or sends its requests somewhere other than OpenAI.
///
/// Read from the versions Mjolnir pins (codex-acp 1.13.2 with codex 0.156.1):
/// the bridge's API-key login reads `CODEX_API_KEY` and then `OPENAI_API_KEY`;
/// the codex binary lists `OPENAI_API_KEY`, `CODEX_API_KEY` and
/// `CODEX_ACCESS_TOKEN` as the variables it accepts in place of `auth.json`;
/// and `OPENAI_BASE_URL` sets the address of Codex's built-in OpenAI provider.
pub const CODEX_CREDENTIAL_ENVIRONMENT: [&str; 4] = [
    "CODEX_ACCESS_TOKEN",
    "CODEX_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
];

/// How a Codex profile that talks to OpenAI itself signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexLogin {
    /// A ChatGPT account: an OAuth grant in `auth.json` that Codex refreshes.
    ChatGpt,
    /// An OpenAI API key that `codex login --with-api-key` stored in
    /// `auth.json`.
    ApiKey,
}

impl HarnessProfile {
    /// How this profile signs in to OpenAI. `None` for another harness and for
    /// providers that do not use Codex's own OpenAI login.
    ///
    /// Codex records the choice as `auth_mode` in `auth.json`: `"apikey"`
    /// after `codex login --with-api-key` and `"chatgpt"` after a ChatGPT
    /// login. Anything but `"apikey"`, a missing file included, counts as a
    /// ChatGPT login, because such a profile has no API key of its own: a key
    /// could only reach it from somewhere else.
    pub fn codex_login(&self) -> Option<CodexLogin> {
        if self.kind != HarnessKind::Codex
            || self
                .codex_provider()
                .ok()
                .flatten()
                .is_some_and(|provider| !provider.uses_codex_login())
        {
            return None;
        }
        let auth_mode = std::fs::read(harness_authentication_marker(self.kind, &self.home))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|login| login.get("auth_mode")?.as_str().map(str::to_owned));
        Some(match auth_mode.as_deref() {
            Some("apikey") => CodexLogin::ApiKey,
            _ => CodexLogin::ChatGpt,
        })
    }

    /// Remove from `environment` every variable this profile's harness must
    /// never see, and return the names of all such variables, present or not.
    ///
    /// A Codex profile that signs in with ChatGPT, or selects a built-in
    /// provider that does not use OpenAI credentials, must not inherit an API
    /// key from its environment. The names travel in the launch description
    /// as well, because the worker adds the target's own login environment
    /// later and has to remove them from that too.
    pub fn exclude_harness_environment(
        &self,
        environment: &mut BTreeMap<String, String>,
    ) -> Vec<String> {
        let built_in_without_openai_auth = self
            .codex_provider()
            .ok()
            .flatten()
            .and_then(|provider| provider.built_in())
            .is_some_and(|provider| !provider.uses_codex_login());
        if self.codex_login() != Some(CodexLogin::ChatGpt) && !built_in_without_openai_auth {
            return Vec::new();
        }
        CODEX_CREDENTIAL_ENVIRONMENT
            .iter()
            .map(|name| {
                environment.remove(*name);
                (*name).to_owned()
            })
            .collect()
    }
}

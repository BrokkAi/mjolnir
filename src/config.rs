//! Persistent user config for `mj`.
//!
//! Stores the primary agent and subagent-pool preferences plus custom ACP
//! launches. Lives at `~/.config/mj/config.toml`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{expand_home_shortcut, normalize_spawn_program};
use crate::spinner::SpinnerStyle;
use crate::theme::TerminalThemeKind;

pub const DISABLED_MODEL: &str = "disabled";
pub const CONFIG_VERSION: u32 = 3;
/// Version of the product-model explanation accepted by the user. This is
/// intentionally independent from the storage schema version.
pub const ONBOARDING_CONTENT_VERSION: u32 = 4;
pub const DEFAULT_ACP_PRIORITY: [&str; 2] = ["codex-acp", "claude-acp"];
/// Schema version this build can migrate forward from.
const MIGRATABLE_VERSION: u32 = 2;

/// Saved ACP session defaults are scoped to the seat that will consume them.
/// Live accepted values remain in the top-level `session_config` cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionConfigSeat {
    Primary,
    Subagent,
    Review,
}

/// Per-invocation model overrides (`--model` / `--review-model` /
/// `--subagent-model`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOverrides {
    pub primary: Option<String>,
    pub primary_effort: Option<String>,
    pub review: Option<String>,
    pub review_effort: Option<String>,
    pub subagent: Option<String>,
    pub subagent_effort: Option<String>,
}

/// Amount of agent thought text shown in the normal transcript view.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThoughtOutput {
    /// Preserve the compact transcript: completed thoughts become summaries
    /// and an active thought shows only its latest bounded tail.
    #[default]
    Current,
    /// Render every available line of agent thought text.
    Full,
}

impl ThoughtOutput {
    pub const ALL: [Self; 2] = [Self::Current, Self::Full];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Full => "full",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Current => "summarize completed thoughts; show the latest live thought",
            Self::Full => "show all available thought output",
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Display for ThoughtOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ThoughtOutput {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "current" => Ok(Self::Current),
            "full" => Ok(Self::Full),
            _ => Err(format!(
                "unknown thought output {value:?}; expected one of: {}",
                Self::ALL
                    .iter()
                    .map(|output| output.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    pub version: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub onboarding_version: u32,
    #[serde(default, skip_serializing_if = "TerminalThemeKind::is_default")]
    pub theme: TerminalThemeKind,
    #[serde(default, skip_serializing_if = "SpinnerStyle::is_default")]
    pub spinner: SpinnerStyle,
    /// Amount of thought text shown in terminal and web transcripts.
    #[serde(default, skip_serializing_if = "ThoughtOutput::is_default")]
    pub thought_output: ThoughtOutput,
    /// Show occasional capability-aware tips between completed turns.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub feature_hints: bool,
    /// Keep the system awake while mj is working: the whole time `mj server`
    /// runs, and while a terminal session has a turn in flight.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub keep_awake: bool,
    /// The primary agent's model and review behavior.
    #[serde(default, skip_serializing_if = "AgentConfig::is_default")]
    pub agent: AgentConfig,
    /// The discrete review supervisor's model preference.
    #[serde(default, skip_serializing_if = "ReviewConfig::is_default")]
    pub review: ReviewConfig,
    /// Defaults for the shared subagent pool.
    #[serde(default, skip_serializing_if = "SubagentsConfig::is_default")]
    pub subagents: SubagentsConfig,
    /// ACP adapter enablement and explicit user-provisioned servers.
    #[serde(default, skip_serializing_if = "AcpConfig::is_default")]
    pub acp: AcpConfig,
    /// ACP session option overrides, keyed by ACP server id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_config: BTreeMap<String, AcpSessionConfig>,
    /// `/ragnarok` battle knobs.
    #[serde(default, skip_serializing_if = "RagnarokConfig::is_default")]
    pub ragnarok: RagnarokConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcpSessionConfig {
    /// Defaults chosen in `/mjconfig` for future sessions on this server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<String, String>,
    /// Values accepted by live sessions, keyed by configured model identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            onboarding_version: 0,
            theme: TerminalThemeKind::default(),
            spinner: SpinnerStyle::default(),
            thought_output: ThoughtOutput::default(),
            feature_hints: true,
            keep_awake: true,
            agent: AgentConfig::default(),
            review: ReviewConfig::default(),
            subagents: SubagentsConfig::default(),
            acp: AcpConfig::default(),
            session_config: BTreeMap::new(),
            ragnarok: RagnarokConfig::default(),
        }
    }
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Permission preset applied to an ACP runtime. Never persisted: interactive
/// and remote sessions inherit the ACP harness policy, and headless sessions
/// pass `--permission-mode` through directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PermissionPreset {
    Manual,
    #[default]
    Auto,
    Yolo,
}

impl std::fmt::Display for PermissionPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Manual => "Manual",
            Self::Auto => "Auto",
            Self::Yolo => "YOLO",
        })
    }
}

fn default_auto() -> String {
    "auto".to_string()
}

fn default_acp_priority() -> Vec<String> {
    DEFAULT_ACP_PRIORITY
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn is_default_acp_priority(priority: &[String]) -> bool {
    priority.iter().map(String::as_str).eq(DEFAULT_ACP_PRIORITY)
}

/// The model and resolved ACP source currently bound to each seat, for display
/// only. Configured (not yet running) selections leave the sources absent.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelsConfig {
    #[serde(default = "default_auto")]
    pub primary: String,
    #[serde(default = "default_auto")]
    pub review: String,
    #[serde(default = "default_auto")]
    pub subagent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_source: Option<String>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            primary: default_auto(),
            review: default_auto(),
            subagent: default_auto(),
            primary_source: None,
            review_source: None,
            subagent_source: None,
        }
    }
}

/// One of the supported coding/review provider combinations.
///
/// A team pins the primary and subagent seats to its coder and the discrete
/// review seat to its reviewer. Models remain automatic within those sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamPreset {
    Codex,
    Claude,
    CodexWithClaudeReviewer,
    ClaudeWithCodexReviewer,
}

impl TeamPreset {
    pub const ALL: [Self; 4] = [
        Self::Codex,
        Self::Claude,
        Self::CodexWithClaudeReviewer,
        Self::ClaudeWithCodexReviewer,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::CodexWithClaudeReviewer => "codex_claude",
            Self::ClaudeWithCodexReviewer => "claude_codex",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::CodexWithClaudeReviewer => "Codex coder + Claude reviewer",
            Self::ClaudeWithCodexReviewer => "Claude coder + Codex reviewer",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Codex => "Codex codes, delegates, and reviews",
            Self::Claude => "Claude codes, delegates, and reviews",
            Self::CodexWithClaudeReviewer => "Codex codes and delegates; Claude reviews",
            Self::ClaudeWithCodexReviewer => "Claude codes and delegates; Codex reviews",
        }
    }

    pub const fn sources(self) -> (&'static str, &'static str) {
        match self {
            Self::Codex => ("codex-acp", "codex-acp"),
            Self::Claude => ("claude-acp", "claude-acp"),
            Self::CodexWithClaudeReviewer => ("codex-acp", "claude-acp"),
            Self::ClaudeWithCodexReviewer => ("claude-acp", "codex-acp"),
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == id)
    }

    pub fn from_config(config: &Config) -> Option<Self> {
        let coder = config.agent.acp_source.as_deref()?;
        if config.subagents.acp_source.as_deref() != Some(coder) {
            return None;
        }
        let reviewer = config.review.acp_source.as_deref()?;
        Self::ALL
            .into_iter()
            .find(|preset| preset.sources() == (coder, reviewer))
    }

    pub fn apply(self, config: &mut Config) {
        let (coder, reviewer) = self.sources();
        config.agent.model = default_auto();
        config.agent.acp_source = Some(coder.to_string());
        config.agent.discrete_review = true;
        config.review.model = default_auto();
        config.review.acp_source = Some(reviewer.to_string());
        config.subagents.model = default_auto();
        config.subagents.acp_source = Some(coder.to_string());
        config.subagents.auto_failover = true;
        for source in [coder, reviewer] {
            config.set_acp_server_policy(source, AcpServerPolicy::Enabled);
        }
    }
}

/// How much machinery one discrete review is allowed to spend.
///
/// `Quick` runs a single general reviewer and then validates its findings,
/// which is the cheap default. `Extended` runs the full adversarial
/// supervisor with its on-demand Norse specialist roster.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReviewTier {
    #[default]
    Quick,
    Extended,
}

impl ReviewTier {
    pub const ALL: [Self; 2] = [Self::Quick, Self::Extended];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Extended => "extended",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Quick => "Quick",
            Self::Extended => "Extended",
        }
    }

    /// One line of `/mjconfig` help describing what the tier actually spends.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Quick => "one general reviewer, then a validation pass over its findings",
            Self::Extended => {
                "adversarial supervisor with on-demand Norse specialist lanes; far more tokens"
            }
        }
    }

    /// Compact representation for the orchestrator's atomic live switch.
    pub const fn as_index(self) -> u8 {
        match self {
            Self::Quick => 0,
            Self::Extended => 1,
        }
    }

    /// Unknown indexes fall back to the cheap tier: an unreadable switch must
    /// never silently upgrade a user into the expensive review.
    pub const fn from_index(index: u8) -> Self {
        match index {
            1 => Self::Extended,
            _ => Self::Quick,
        }
    }

    fn is_default(&self) -> bool {
        matches!(self, Self::Quick)
    }
}

impl std::fmt::Display for ReviewTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReviewTier {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|tier| tier.as_str().eq_ignore_ascii_case(value))
            .ok_or(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentConfig {
    #[serde(default = "default_auto")]
    pub model: String,
    /// Restrict this seat to one ACP source while retaining automatic model
    /// selection within that source. `None` allows every enabled source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_source: Option<String>,
    /// Preferred ACP sources when more than one enabled adapter offers the
    /// selected model. Unlisted sources follow in discovery order.
    #[serde(
        default = "default_acp_priority",
        skip_serializing_if = "is_default_acp_priority"
    )]
    pub acp_priority: Vec<String>,
    /// Reasoning-effort override for the primary agent's ACP session. It may
    /// be supplied for one `--print` invocation or saved from the interactive
    /// primary model picker for future sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Adapter-owned session defaults selected for future primary sessions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_defaults: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default = "default_true")]
    pub discrete_review: bool,
    /// How much review machinery each discrete review spends. Absent from an
    /// older config means the cheap default, so upgrading users land on
    /// `Quick` without editing anything.
    #[serde(default, skip_serializing_if = "ReviewTier::is_default")]
    pub review_tier: ReviewTier,
    /// How many corrective re-review passes one user turn may dispatch after
    /// its initial discrete review. `0` accepts the first correction without
    /// re-reviewing it; the default spends exactly one bounded verification
    /// pass, which is what stops findings-correction from re-arming forever.
    #[serde(default = "default_max_correction_rounds")]
    pub max_correction_rounds: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: default_auto(),
            acp_source: None,
            acp_priority: default_acp_priority(),
            reasoning_effort: None,
            session_defaults: BTreeMap::new(),
            discrete_review: true,
            review_tier: ReviewTier::default(),
            max_correction_rounds: default_max_correction_rounds(),
        }
    }
}

impl AgentConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReviewConfig {
    #[serde(default = "default_auto")]
    pub model: String,
    /// Restrict this seat to one ACP source while retaining automatic model
    /// selection within that source. `None` allows every enabled source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_source: Option<String>,
    /// Preferred ACP sources when more than one enabled adapter offers the
    /// selected review supervisor model. Unlisted sources follow in discovery
    /// order.
    #[serde(
        default = "default_acp_priority",
        skip_serializing_if = "is_default_acp_priority"
    )]
    pub acp_priority: Vec<String>,
    /// Reasoning-effort default for review ACP sessions. A one-shot
    /// `--review-model MODEL+high` override replaces it only for that run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Adapter-owned session defaults selected for future review sessions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_defaults: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            model: default_auto(),
            acp_source: None,
            acp_priority: default_acp_priority(),
            reasoning_effort: None,
            session_defaults: BTreeMap::new(),
        }
    }
}

impl ReviewConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SubagentsConfig {
    #[serde(default = "default_auto")]
    pub model: String,
    /// Restrict this seat and its automatic failover pool to one ACP source.
    /// `None` allows every enabled source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_source: Option<String>,
    /// Preferred ACP sources when more than one enabled adapter offers the
    /// selected worker model. Unlisted sources follow in discovery order.
    #[serde(
        default = "default_acp_priority",
        skip_serializing_if = "is_default_acp_priority"
    )]
    pub acp_priority: Vec<String>,
    /// Reasoning-effort default for delegated ACP sessions. A one-shot
    /// `--subagent-model MODEL+high` override replaces it only for that run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Adapter-owned session defaults selected for future delegated sessions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_defaults: BTreeMap<String, BTreeMap<String, String>>,
    /// Concurrency cap for the shared subagent pool.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
    /// Move the pool to the next route when an ACP source nears its quota.
    #[serde(default = "default_true")]
    pub auto_failover: bool,
    /// Ask completed pool subagents for a terse exit interview before report delivery.
    #[serde(default = "default_true")]
    pub debrief: bool,
    /// Minutes a primary parked on running subagents may go without a report
    /// before it is woken with their progress alone. `0` disables the wake.
    #[serde(default = "default_progress_wake_minutes")]
    pub progress_wake_minutes: u64,
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            model: default_auto(),
            acp_source: None,
            acp_priority: default_acp_priority(),
            reasoning_effort: None,
            session_defaults: BTreeMap::new(),
            max_parallel: default_max_parallel(),
            auto_failover: true,
            debrief: true,
            progress_wake_minutes: default_progress_wake_minutes(),
        }
    }
}

fn default_max_parallel() -> usize {
    6
}

fn default_max_correction_rounds() -> u32 {
    1
}

fn default_progress_wake_minutes() -> u64 {
    20
}

impl SubagentsConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcpConfig {
    /// Policy overrides for built-in auto-detected servers. Missing means Auto.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, AcpServerPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<ConfiguredAcpServer>,
}

impl AcpConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcpServerPolicy {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

impl std::fmt::Display for AcpServerPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Enabled => f.write_str("on"),
            Self::Disabled => f.write_str("off"),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcpServerOrigin {
    #[serde(alias = "registry")]
    Custom,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredAcpServer {
    pub id: String,
    pub label: String,
    pub command: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    pub origin: AcpServerOrigin,
    #[serde(default = "enabled_policy", skip_serializing_if = "is_enabled_policy")]
    pub policy: AcpServerPolicy,
}

fn enabled_policy() -> AcpServerPolicy {
    AcpServerPolicy::Enabled
}

fn is_enabled_policy(policy: &AcpServerPolicy) -> bool {
    *policy == AcpServerPolicy::Enabled
}

/// Knobs for `/ragnarok` battles.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RagnarokConfig {
    /// Hard cap on how many champions Thor may field (2-10). Thor still
    /// decides the count from task complexity; this caps the bill.
    #[serde(default = "default_max_competitors")]
    pub max_competitors: usize,
}

fn default_max_competitors() -> usize {
    10
}

impl Default for RagnarokConfig {
    fn default() -> Self {
        Self {
            max_competitors: default_max_competitors(),
        }
    }
}

impl RagnarokConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_true() -> bool {
    true
}

/// Concrete ACP launch selected by the model catalog for a session.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectedAgent {
    pub source_id: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

impl Config {
    /// True when `path` holds a config this build can use directly or migrate
    /// forward. Callers use it to decide whether the user is already
    /// onboarded, so a migratable v2 file counts as an existing config.
    pub fn path_has_current_version(path: &Path) -> bool {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return false;
        };
        matches!(
            toml::from_str::<toml::Value>(&contents)
                .ok()
                .and_then(|document| document.get("version").and_then(toml::Value::as_integer)),
            Some(version)
                if version == i64::from(CONFIG_VERSION)
                    || version == i64::from(MIGRATABLE_VERSION)
        )
    }

    pub fn apply_model_overrides(&mut self, overrides: &ModelOverrides) {
        if let Some(model) = &overrides.primary {
            self.agent.model.clone_from(model);
            self.agent.acp_source = None;
            self.agent.reasoning_effort = overrides.primary_effort.clone();
        }
        if let Some(model) = &overrides.review {
            self.review.model.clone_from(model);
            self.review.acp_source = None;
            self.review.reasoning_effort = overrides.review_effort.clone();
        }
        if let Some(model) = &overrides.subagent {
            self.subagents.model.clone_from(model);
            self.subagents.acp_source = None;
            self.subagents.reasoning_effort = overrides.subagent_effort.clone();
        }
    }

    /// Forget settings that named an ACP source this build no longer ships, so
    /// an older config keeps launching instead of failing on a dangling pin.
    /// A seat pinned to a retired source, or to a model whose provider no
    /// built-in adapter serves, falls back to automatic selection.
    fn drop_retired_sources(&mut self) {
        let known = DEFAULT_ACP_PRIORITY
            .iter()
            .map(|id| (*id).to_string())
            .chain(self.acp.servers.iter().map(|server| server.id.clone()))
            .collect::<std::collections::HashSet<_>>();
        let retired_model = |model: &str| {
            !matches!(model, "auto" | DISABLED_MODEL | "none")
                && !model.starts_with("custom/")
                && !crate::roster::model_has_builtin_adapter(model)
        };

        self.acp.policies.retain(|id, _| known.contains(id));
        for (source, priority, model) in [
            (
                &mut self.agent.acp_source,
                &mut self.agent.acp_priority,
                &mut self.agent.model,
            ),
            (
                &mut self.review.acp_source,
                &mut self.review.acp_priority,
                &mut self.review.model,
            ),
            (
                &mut self.subagents.acp_source,
                &mut self.subagents.acp_priority,
                &mut self.subagents.model,
            ),
        ] {
            priority.retain(|id| known.contains(id));
            if source.as_deref().is_some_and(|id| !known.contains(id)) {
                *source = None;
            }
            if retired_model(model.as_str()) {
                "auto".clone_into(model);
            }
        }
    }

    pub fn set_acp_server_policy(&mut self, id: &str, policy: AcpServerPolicy) -> bool {
        if matches!(id, "codex-acp" | "claude-acp") {
            if policy == AcpServerPolicy::Auto {
                self.acp.policies.remove(id);
            } else {
                self.acp.policies.insert(id.to_string(), policy);
            }
            return true;
        }
        let Some(server) = self.acp.servers.iter_mut().find(|server| server.id == id) else {
            return false;
        };
        server.policy = policy;
        true
    }

    pub fn model_names(&self) -> ModelsConfig {
        ModelsConfig {
            primary: self.agent.model.clone(),
            review: self.review.model.clone(),
            subagent: self.subagents.model.clone(),
            primary_source: None,
            review_source: None,
            subagent_source: None,
        }
    }

    /// Read the config from `path`. Returns `Config::default()` when the
    /// file does not exist; surfaces a parse error otherwise. A `version = 2`
    /// file is migrated to the current schema and written back in place.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let s =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let document: toml::Value =
            toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?;
        let version = document.get("version").and_then(toml::Value::as_integer);
        if version == Some(i64::from(MIGRATABLE_VERSION)) {
            let mut cfg = migrate_v2(&s).with_context(|| format!("migrate {}", path.display()))?;
            cfg.normalize()?;
            if let Err(error) = cfg.save(path) {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "config migrated to v3 in memory but could not be written back"
                );
            }
            return Ok(cfg);
        }
        if version != Some(i64::from(CONFIG_VERSION)) {
            tracing::warn!(
                path = %path.display(),
                found_version = ?version,
                expected_version = CONFIG_VERSION,
                "ignoring incompatible config and starting fresh"
            );
            return Ok(Self::default());
        }
        let mut cfg: Self =
            toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?;
        cfg.normalize()?;
        Ok(cfg)
    }

    /// Atomic-ish save: write to a tmp sibling then rename. Creates the
    /// parent directory on demand.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialize config")?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary config in {}", parent.display()))?;
        std::io::Write::write_all(&mut tmp, body.as_bytes())
            .with_context(|| format!("write temporary config in {}", parent.display()))?;
        tmp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    fn normalize(&mut self) -> Result<()> {
        if self.subagents.model.eq_ignore_ascii_case("none") {
            self.subagents.model = DISABLED_MODEL.to_string();
        }
        self.drop_retired_sources();
        for (seat, priority) in [
            ("agent", &self.agent.acp_priority),
            ("review", &self.review.acp_priority),
            ("subagents", &self.subagents.acp_priority),
        ] {
            let mut seen = std::collections::HashSet::new();
            for source_id in priority {
                anyhow::ensure!(
                    !source_id.trim().is_empty(),
                    "{seat}.acp_priority contains an empty source id"
                );
                anyhow::ensure!(
                    seen.insert(source_id),
                    "{seat}.acp_priority contains duplicate source id '{source_id}'"
                );
            }
        }
        for (seat, source) in [
            ("agent", self.agent.acp_source.as_deref()),
            ("review", self.review.acp_source.as_deref()),
            ("subagents", self.subagents.acp_source.as_deref()),
        ] {
            anyhow::ensure!(
                source.is_none_or(|source| !source.trim().is_empty()),
                "{seat}.acp_source cannot be empty"
            );
        }
        anyhow::ensure!(
            self.subagents.max_parallel <= 16,
            "subagents.max_parallel must be between 0 and 16"
        );

        let mut names = std::collections::HashSet::new();
        for server in &mut self.acp.servers {
            let valid_name = !server.id.is_empty()
                && server
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'));
            anyhow::ensure!(
                valid_name,
                "ACP server id '{}' contains unsupported characters",
                server.id
            );
            anyhow::ensure!(
                names.insert(server.id.clone()),
                "duplicate configured ACP server id '{}'",
                server.id
            );
            anyhow::ensure!(
                !server.command.as_os_str().is_empty(),
                "configured ACP server '{}' has an empty command",
                server.id
            );
            server.command = expand_home_shortcut(&server.command.to_string_lossy());
            server.command = normalize_spawn_program(server.command.clone());
            server.args = server
                .args
                .iter()
                .map(|arg| expand_home_shortcut(arg).to_string_lossy().into_owned())
                .collect();
        }
        Ok(())
    }
}

impl AcpConfig {
    pub fn policy(&self, id: &str) -> AcpServerPolicy {
        self.policies.get(id).copied().unwrap_or_default()
    }
}

/// The v2 (`[thor]`/`[eitri]`/`[loki]`/`[council]`) schema, parsed leniently so
/// a stale file never blocks startup. Unknown keys are ignored; sections that
/// survived the schema change (`theme`, `spinner`, `acp`, `ragnarok`) are
/// carried over verbatim by reusing their current types.
#[derive(Debug, Default, Deserialize)]
struct ConfigV2 {
    #[serde(default)]
    theme: TerminalThemeKind,
    #[serde(default)]
    spinner: SpinnerStyle,
    #[serde(default)]
    thor: ThorV2,
    #[serde(default)]
    loki: LokiV2,
    #[serde(default)]
    eitri: EitriV2,
    #[serde(default)]
    council: CouncilV2,
    #[serde(default)]
    acp: AcpConfig,
    #[serde(default)]
    ragnarok: RagnarokConfig,
}

#[derive(Debug, Deserialize)]
struct ThorV2 {
    #[serde(default = "default_auto")]
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default = "default_true")]
    discrete_review: bool,
    #[serde(default = "default_max_correction_rounds")]
    max_correction_rounds: u32,
}

impl Default for ThorV2 {
    fn default() -> Self {
        Self {
            model: default_auto(),
            reasoning_effort: None,
            discrete_review: true,
            max_correction_rounds: default_max_correction_rounds(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct LokiV2 {
    #[serde(default = "default_auto")]
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

impl Default for LokiV2 {
    fn default() -> Self {
        Self {
            model: default_auto(),
            reasoning_effort: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct EitriV2 {
    #[serde(default = "default_auto")]
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default = "default_max_parallel")]
    max_parallel_explores: usize,
    #[serde(default = "default_true")]
    debrief: bool,
    #[serde(default = "default_progress_wake_minutes")]
    progress_wake_minutes: u64,
}

impl Default for EitriV2 {
    fn default() -> Self {
        Self {
            model: default_auto(),
            reasoning_effort: None,
            max_parallel_explores: default_max_parallel(),
            debrief: true,
            progress_wake_minutes: default_progress_wake_minutes(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CouncilV2 {
    #[serde(default = "default_true")]
    auto_failover: bool,
}

impl Default for CouncilV2 {
    fn default() -> Self {
        Self {
            auto_failover: true,
        }
    }
}

/// Map a `version = 2` document onto the current schema.
/// `council.permission_mode` is dropped: the permission preset is no longer
/// persisted.
fn migrate_v2(body: &str) -> Result<Config> {
    let old: ConfigV2 = toml::from_str(body).context("parse v2 config")?;
    Ok(Config {
        version: CONFIG_VERSION,
        onboarding_version: 0,
        theme: old.theme,
        spinner: old.spinner,
        thought_output: ThoughtOutput::default(),
        feature_hints: true,
        keep_awake: true,
        agent: AgentConfig {
            model: old.thor.model,
            acp_source: None,
            acp_priority: default_acp_priority(),
            reasoning_effort: old.thor.reasoning_effort,
            session_defaults: BTreeMap::new(),
            discrete_review: old.thor.discrete_review,
            review_tier: ReviewTier::default(),
            max_correction_rounds: old.thor.max_correction_rounds,
        },
        review: ReviewConfig {
            model: old.loki.model,
            acp_source: None,
            acp_priority: default_acp_priority(),
            reasoning_effort: old.loki.reasoning_effort,
            session_defaults: BTreeMap::new(),
        },
        subagents: SubagentsConfig {
            model: old.eitri.model,
            acp_source: None,
            acp_priority: default_acp_priority(),
            reasoning_effort: old.eitri.reasoning_effort,
            session_defaults: BTreeMap::new(),
            max_parallel: old.eitri.max_parallel_explores,
            auto_failover: old.council.auto_failover,
            debrief: old.eitri.debrief,
            progress_wake_minutes: old.eitri.progress_wake_minutes,
        },
        acp: old.acp,
        session_config: BTreeMap::new(),
        ragnarok: old.ragnarok,
    })
}

/// Default config path: `$XDG_CONFIG_HOME/mj/config.toml` (or
/// `~/.config/mj/config.toml` when `XDG_CONFIG_HOME` is unset).
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("config.toml")
}

pub fn load_saved_session_config(
    path: &Path,
    source_id: &str,
    model_id: &str,
    seat: SessionConfigSeat,
) -> HashMap<String, String> {
    match Config::load(path) {
        Ok(config) => {
            let mut values = HashMap::new();
            if let Some(saved) = config.session_config.get(source_id) {
                values.extend(saved.defaults.clone());
                if let Some(route) = saved.models.get(model_id) {
                    values.extend(route.clone());
                }
            }
            let scoped = match seat {
                SessionConfigSeat::Primary => config.agent.session_defaults.get(source_id),
                SessionConfigSeat::Subagent => config.subagents.session_defaults.get(source_id),
                SessionConfigSeat::Review => config.review.session_defaults.get(source_id),
            };
            if let Some(scoped) = scoped {
                values.extend(scoped.clone());
            }
            values
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                adapter = source_id,
                "could not load saved ACP session config: {error:#}"
            );
            HashMap::new()
        }
    }
}

static SESSION_CONFIG_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn save_user_config_preserving_session_routes(path: &Path, config: &mut Config) -> Result<()> {
    let _guard = SESSION_CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let latest = Config::load(path)?;
    for (source_id, saved) in latest.session_config {
        let changed_defaults = config
            .session_config
            .get(&source_id)
            .map(|edited| {
                edited
                    .defaults
                    .iter()
                    .filter(|(key, value)| saved.defaults.get(*key) != Some(*value))
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !saved.models.is_empty() {
            let routes = &mut config.session_config.entry(source_id).or_default().models;
            routes.clone_from(&saved.models);
            for route in routes.values_mut() {
                for key in &changed_defaults {
                    route.remove(key);
                }
            }
        }
    }
    config.save(path)
}

pub fn persist_accepted_session_config(
    path: &Path,
    source_id: &str,
    model_id: &str,
    key: String,
    value: String,
) -> Result<()> {
    let _guard = SESSION_CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut config = Config::load(path)?;
    config
        .session_config
        .entry(source_id.to_string())
        .or_default()
        .models
        .entry(model_id.to_string())
        .or_default()
        .insert(key, value);
    config.save(path)
}

/// Directory for exported conversation transcripts:
/// `$XDG_CONFIG_HOME/mj/transcripts`.
pub fn transcript_export_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("mj").join("transcripts"))
}

/// Path for the persisted prompt-history file (NUL-delimited format to
/// support multiline prompts): `$XDG_CONFIG_HOME/mj/history.txt`.
pub fn history_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("history.txt")
}

/// Maximum number of history entries kept on disk. Older entries are
/// trimmed when the limit is exceeded.
pub const HISTORY_MAX_ENTRIES: usize = 100;

/// Load the prompt history from a NUL-delimited file (supports multiline
/// prompts). Returns an empty `Vec` when the file does not exist or is
/// unreadable.
pub fn load_history(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path).map_err(|e| tracing::warn!("load_history {path:?}: {e}")) {
        Ok(body) => body
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Persist the prompt history to disk in NUL-delimited format, capped
/// at `HISTORY_MAX_ENTRIES`.
pub fn save_history(path: &Path, entries: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create history dir {}", parent.display()))?;
    }
    let tail = if entries.len() > HISTORY_MAX_ENTRIES {
        &entries[entries.len() - HISTORY_MAX_ENTRIES..]
    } else {
        entries
    };
    let body = tail.join("\0");
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_history_returns_empty_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries = load_history(&path);
        assert!(entries.is_empty());
    }

    #[test]
    fn load_save_history_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries: Vec<String> = (0..5).map(|i| format!("prompt {i}")).collect();
        save_history(&path, &entries).expect("save");
        let loaded = load_history(&path);
        assert_eq!(loaded, entries);
    }

    #[test]
    fn save_history_caps_at_max_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries: Vec<String> = (0..120).map(|i| format!("prompt {i}")).collect();
        save_history(&path, &entries).expect("save");
        let loaded = load_history(&path);
        assert_eq!(loaded.len(), HISTORY_MAX_ENTRIES);
        // Keeps the most recent entries (tail).
        assert_eq!(loaded[0], format!("prompt {}", 120 - HISTORY_MAX_ENTRIES));
        assert_eq!(loaded[loaded.len() - 1], "prompt 119");
    }

    #[test]
    fn save_history_creates_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("history.txt");
        save_history(&path, &["hi".to_string()]).expect("save");
        assert_eq!(load_history(&path), vec!["hi".to_string()]);
    }

    #[test]
    fn save_load_history_preserves_multiline_prompts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        let entries = vec![
            "single line".to_string(),
            "line one\nline two\nline three".to_string(),
            "another single".to_string(),
        ];
        save_history(&path, &entries).expect("save");
        let loaded = load_history(&path);
        assert_eq!(loaded, entries);
    }

    #[test]
    fn save_empty_history_writes_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.txt");
        save_history(&path, &[]).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert_eq!(body, "");
        let loaded = load_history(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn ragnarok_max_competitors_roundtrips_and_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        // Default cap is omitted from the serialized form.
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            !body.contains("ragnarok"),
            "default ragnarok config should not be serialized: {body:?}"
        );
        assert_eq!(
            Config::load(&path).expect("load").ragnarok.max_competitors,
            10
        );

        // A custom cap survives the round trip.
        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[ragnarok]\nmax_competitors = 3\n"),
        )
        .expect("write");
        let cfg = Config::load(&path).expect("load custom");
        assert_eq!(cfg.ragnarok.max_competitors, 3);
        cfg.save(&path).expect("save custom");
        let body = std::fs::read_to_string(&path).expect("read saved");
        assert!(body.contains("max_competitors = 3"), "body: {body:?}");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.toml");
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.theme, TerminalThemeKind::Adaptive);
        assert_eq!(cfg.model_names(), ModelsConfig::default());
        assert!(cfg.agent.discrete_review);
        assert_eq!(cfg.agent.max_correction_rounds, 1);
        assert_eq!(cfg.subagents.model, "auto");
        assert_eq!(
            cfg.agent.acp_priority,
            DEFAULT_ACP_PRIORITY.map(str::to_string)
        );
        assert_eq!(cfg.agent.acp_priority, cfg.review.acp_priority);
        assert_eq!(cfg.agent.acp_priority, cfg.subagents.acp_priority);
    }

    #[test]
    fn review_tier_defaults_to_quick_and_persists_only_when_upgraded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        // A config written before review tiers existed keeps automatic review
        // on and lands on the cheap tier.
        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[agent]\nmodel = \"gpt-5-6-sol\"\n"),
        )
        .expect("write");
        let cfg = Config::load(&path).expect("load");
        assert!(cfg.agent.discrete_review);
        assert_eq!(cfg.agent.review_tier, ReviewTier::Quick);

        // The default stays out of the file; an explicit upgrade is written.
        cfg.save(&path).expect("save quick");
        let body = std::fs::read_to_string(&path).expect("read quick");
        assert!(!body.contains("review_tier"), "body: {body:?}");

        let mut upgraded = cfg;
        upgraded.agent.review_tier = ReviewTier::Extended;
        upgraded.save(&path).expect("save extended");
        let body = std::fs::read_to_string(&path).expect("read extended");
        assert!(
            body.contains("review_tier = \"extended\""),
            "body: {body:?}"
        );
        assert_eq!(
            Config::load(&path).expect("reload").agent.review_tier,
            ReviewTier::Extended
        );
    }

    #[test]
    fn review_tier_parses_its_own_wire_names() {
        for tier in ReviewTier::ALL {
            assert_eq!(tier.as_str().parse::<ReviewTier>(), Ok(tier));
        }
        assert_eq!("EXTENDED".parse::<ReviewTier>(), Ok(ReviewTier::Extended));
        assert!("thorough".parse::<ReviewTier>().is_err());
        // An unreadable live switch degrades to the cheap tier, never up.
        assert_eq!(ReviewTier::from_index(9), ReviewTier::Quick);
        for tier in ReviewTier::ALL {
            assert_eq!(ReviewTier::from_index(tier.as_index()), tier);
        }
    }

    #[test]
    fn onboarding_content_version_roundtrips_independently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            onboarding_version: ONBOARDING_CONTENT_VERSION,
            ..Config::default()
        };
        cfg.save(&path).expect("save");

        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains(&format!(
                "onboarding_version = {ONBOARDING_CONTENT_VERSION}"
            )),
            "body: {body:?}"
        );
        assert_eq!(
            Config::load(&path).expect("load").onboarding_version,
            ONBOARDING_CONTENT_VERSION
        );
    }

    #[test]
    fn loading_forgets_settings_that_named_a_retired_acp_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // A config written by an older release that named an ACP source we
        // have since retired. `save` cannot produce this any more, so write
        // the TOML directly.
        std::fs::write(
            &path,
            format!(
                r#"version = {CONFIG_VERSION}

[agent]
model = "glm-5-2"
acp_source = "retired-acp"
acp_priority = ["retired-acp", "codex-acp"]

[review]
model = "auto"
acp_priority = ["codex-acp", "retired-acp"]

[subagents]
model = "gpt-5-6-sol"
acp_source = "codex-acp"

[acp.policies]
retired-acp = "enabled"
kimi = "disabled"
"#
            ),
        )
        .expect("write legacy config");

        let loaded = Config::load(&path).expect("load");

        assert_eq!(loaded.agent.acp_source, None);
        assert_eq!(loaded.agent.acp_priority, vec!["codex-acp".to_string()]);
        assert_eq!(loaded.review.acp_priority, vec!["codex-acp".to_string()]);
        assert!(!loaded.acp.policies.contains_key("retired-acp"));
        // Kimi Code was removed; its persisted policy is dropped like any
        // other retired source.
        assert!(!loaded.acp.policies.contains_key("kimi"));
        // The pinned model's provider has no built-in adapter left either.
        assert_eq!(loaded.agent.model, "auto");
        // Still-served pins are untouched.
        assert_eq!(loaded.subagents.model, "gpt-5-6-sol");
        assert_eq!(loaded.subagents.acp_source.as_deref(), Some("codex-acp"));
    }

    #[test]
    fn loading_keeps_models_served_by_a_configured_custom_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.acp.servers.push(ConfiguredAcpServer {
            id: "custom:bridge".to_string(),
            label: "bridge".to_string(),
            command: PathBuf::from("bridge"),
            args: Vec::new(),
            env: HashMap::new(),
            origin: AcpServerOrigin::Custom,
            policy: AcpServerPolicy::Enabled,
        });
        cfg.agent.model = "custom/bridge/bedrock::zai.glm-5".to_string();
        cfg.agent.acp_source = Some("custom:bridge".to_string());
        cfg.save(&path).expect("save");

        let loaded = Config::load(&path).expect("load");

        assert_eq!(loaded.agent.model, "custom/bridge/bedrock::zai.glm-5");
        assert_eq!(loaded.agent.acp_source.as_deref(), Some("custom:bridge"));
    }

    #[test]
    fn independent_acp_priorities_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.agent.acp_source = Some("codex-acp".into());
        cfg.review.acp_source = Some("claude-acp".into());
        cfg.subagents.acp_source = Some("claude-acp".into());
        cfg.agent.acp_priority = vec!["claude-acp".into(), "codex-acp".into()];
        cfg.review.acp_priority = vec!["claude-acp".into(), "codex-acp".into()];
        cfg.subagents.acp_priority = vec!["codex-acp".into(), "claude-acp".into()];

        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");

        assert_eq!(loaded.agent.acp_source, cfg.agent.acp_source);
        assert_eq!(loaded.review.acp_source, cfg.review.acp_source);
        assert_eq!(loaded.subagents.acp_source, cfg.subagents.acp_source);
        assert_eq!(loaded.agent.acp_priority, cfg.agent.acp_priority);
        assert_eq!(loaded.review.acp_priority, cfg.review.acp_priority);
        assert_eq!(loaded.subagents.acp_priority, cfg.subagents.acp_priority);
    }

    #[test]
    fn versionless_config_starts_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[agent]\ndiscrete_review = false\n\n[subagents]\nmax_parallel = 3\n",
        )
        .expect("write config");

        let cfg = Config::load(&path).expect("load config");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn current_and_migratable_schemas_count_as_an_existing_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n").expect("old config");
        assert!(!Config::path_has_current_version(&path));
        // A v2 file is migrated on load, while the separate content version
        // still lets startup show the major-upgrade explanation.
        std::fs::write(&path, "version = 2\n").expect("v2 config");
        assert!(Config::path_has_current_version(&path));
        Config::default().save(&path).expect("current config");
        assert!(Config::path_has_current_version(&path));
    }

    #[test]
    fn v2_config_migrates_every_mapped_field_and_is_written_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
version = 2
theme = "ansi-light"
spinner = "bars"

[thor]
model = "gpt-5-6-sol"
reasoning_effort = "high"
discrete_review = false
max_correction_rounds = 3

[eitri]
model = "gpt-5-6-terra"
max_parallel_explores = 9
debrief = false
progress_wake_minutes = 5

[loki]
model = "claude-fable-5"
reasoning_effort = "xhigh"

[council]
auto_failover = false
permission_mode = "manual"

[ragnarok]
max_competitors = 4

[acp.policies]
codex-acp = "disabled"

[[acp.servers]]
id = "custom:company"
label = "company"
command = "/usr/local/bin/company-acp"
args = ["--stdio"]
origin = "custom"
"#,
        )
        .expect("write v2 config");

        let cfg = Config::load(&path).expect("migrate v2");
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.theme, TerminalThemeKind::Ansi);
        assert_eq!(cfg.spinner, SpinnerStyle::Bars);
        assert_eq!(cfg.agent.model, "gpt-5-6-sol");
        assert_eq!(cfg.agent.reasoning_effort.as_deref(), Some("high"));
        assert!(!cfg.agent.discrete_review);
        assert_eq!(cfg.agent.max_correction_rounds, 3);
        assert_eq!(cfg.review.model, "claude-fable-5");
        assert_eq!(cfg.review.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(cfg.subagents.model, "gpt-5-6-terra");
        assert_eq!(cfg.subagents.max_parallel, 9);
        assert!(!cfg.subagents.auto_failover);
        assert!(!cfg.subagents.debrief);
        assert_eq!(cfg.subagents.progress_wake_minutes, 5);
        assert_eq!(cfg.ragnarok.max_competitors, 4);
        assert_eq!(cfg.acp.policy("codex-acp"), AcpServerPolicy::Disabled);
        assert_eq!(cfg.acp.servers[0].id, "custom:company");
        assert_eq!(cfg.acp.servers[0].args, vec!["--stdio"]);

        // The migrated file is persisted, so the next load is a plain v3 read.
        let body = std::fs::read_to_string(&path).expect("read migrated");
        println!("--- migrated v3 config.toml ---\n{body}--- end ---");
        assert!(body.contains("version = 3"), "{body}");
        assert!(!body.contains("[thor]"), "{body}");
        assert!(!body.contains("[eitri]"), "{body}");
        assert!(!body.contains("[loki]"), "{body}");
        assert!(!body.contains("[council]"), "{body}");
        assert!(!body.contains("permission_mode"), "{body}");
        assert_eq!(Config::load(&path).expect("reload migrated"), cfg);
    }

    #[test]
    fn v2_migration_keeps_defaults_for_absent_sections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 2\n").expect("write");
        let cfg = Config::load(&path).expect("migrate");
        assert_eq!(
            cfg,
            Config {
                version: CONFIG_VERSION,
                ..Config::default()
            }
        );
    }

    /// The progress heartbeat is config-file only, so absent means the default
    /// and `0` is the documented way to switch it off.
    #[test]
    fn progress_wake_minutes_defaults_to_twenty_and_accepts_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("version = {CONFIG_VERSION}\n")).expect("write");
        assert_eq!(
            Config::load(&path)
                .expect("load")
                .subagents
                .progress_wake_minutes,
            20
        );

        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[subagents]\nprogress_wake_minutes = 0\n"),
        )
        .expect("write");
        assert_eq!(
            Config::load(&path)
                .expect("load")
                .subagents
                .progress_wake_minutes,
            0
        );
    }

    #[test]
    fn v1_config_starts_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n[thor]\nmodel = \"gpt-5-6-sol\"\n").expect("write");
        assert_eq!(Config::load(&path).expect("load"), Config::default());
    }

    /// `--subagent-model none` and `--subagent-model disabled` are the same
    /// switch; a hand-written config gets the same spelling latitude.
    #[test]
    fn subagent_model_none_normalizes_to_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[subagents]\nmodel = \"NoNe\"\n"),
        )
        .expect("write");
        assert_eq!(
            Config::load(&path).expect("load").subagents.model,
            DISABLED_MODEL
        );

        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[subagents]\nmodel = \"disabled\"\n"),
        )
        .expect("write");
        assert_eq!(
            Config::load(&path).expect("load").subagents.model,
            DISABLED_MODEL
        );
    }

    #[test]
    fn max_parallel_above_the_cap_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[subagents]\nmax_parallel = 17\n"),
        )
        .expect("write");
        let error = Config::load(&path).expect_err("cap exceeded");
        assert!(
            error.to_string().contains("subagents.max_parallel"),
            "{error:#}"
        );

        std::fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n[subagents]\nmax_parallel = 16\n"),
        )
        .expect("write");
        assert_eq!(
            Config::load(&path).expect("at cap").subagents.max_parallel,
            16
        );
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let cfg = Config {
            theme: TerminalThemeKind::Ansi,
            agent: AgentConfig {
                model: "gpt-5-6-sol".to_string(),
                acp_source: None,
                acp_priority: default_acp_priority(),
                reasoning_effort: None,
                session_defaults: BTreeMap::new(),
                discrete_review: false,
                review_tier: ReviewTier::Extended,
                max_correction_rounds: default_max_correction_rounds(),
            },
            subagents: SubagentsConfig {
                auto_failover: false,
                ..SubagentsConfig::default()
            },
            acp: AcpConfig {
                servers: vec![ConfiguredAcpServer {
                    id: "custom:company".to_string(),
                    label: "company".to_string(),
                    command: PathBuf::from("/usr/local/bin/company-acp"),
                    args: vec!["--stdio".to_string()],
                    env: HashMap::new(),
                    origin: AcpServerOrigin::Custom,
                    policy: AcpServerPolicy::Enabled,
                }],
                ..AcpConfig::default()
            },
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.theme, TerminalThemeKind::Ansi);
        assert_eq!(loaded.agent.model, "gpt-5-6-sol");
        assert!(!loaded.agent.discrete_review);
        assert_eq!(loaded.agent.review_tier, ReviewTier::Extended);
        assert!(!loaded.subagents.auto_failover);
        assert_eq!(loaded.acp.servers[0].id, "custom:company");
        assert_eq!(loaded.acp.servers[0].args, vec!["--stdio"]);
    }

    #[test]
    fn model_overrides_do_not_mutate_the_source_config() {
        let mut saved = Config::default();
        saved.agent.acp_source = Some("codex-acp".to_string());
        saved.review.acp_source = Some("codex-acp".to_string());
        saved.subagents.acp_source = Some("codex-acp".to_string());
        let mut invocation = saved.clone();
        invocation.apply_model_overrides(&ModelOverrides {
            primary: Some("gpt-test".to_string()),
            primary_effort: Some("high".to_string()),
            review: Some("claude-review".to_string()),
            review_effort: Some("xhigh".to_string()),
            subagent: Some("qwen-test".to_string()),
            subagent_effort: Some("medium".to_string()),
        });

        assert_eq!(saved.agent.acp_source.as_deref(), Some("codex-acp"));
        assert_eq!(invocation.agent.model, "gpt-test");
        assert_eq!(invocation.agent.acp_source, None);
        assert_eq!(invocation.agent.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(invocation.review.model, "claude-review");
        assert_eq!(invocation.review.acp_source, None);
        assert_eq!(invocation.review.reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(invocation.subagents.model, "qwen-test");
        assert_eq!(invocation.subagents.acp_source, None);
        assert_eq!(
            invocation.subagents.reasoning_effort.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn model_overrides_without_effort_leave_reasoning_effort_unset() {
        let mut invocation = Config::default();
        invocation.apply_model_overrides(&ModelOverrides {
            primary: Some("deepseek-v4-pro".to_string()),
            primary_effort: None,
            review: None,
            review_effort: None,
            subagent: None,
            subagent_effort: None,
        });

        assert_eq!(invocation.agent.model, "deepseek-v4-pro");
        assert_eq!(invocation.agent.reasoning_effort, None);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deep").join("config.toml");
        let cfg = Config {
            theme: TerminalThemeKind::Adaptive,
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        assert!(path.exists());
    }

    #[test]
    fn session_config_round_trips_per_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.session_config
            .entry("codex-acp".to_string())
            .or_default()
            .defaults
            .insert("config:service_tier".to_string(), "priority".to_string());

        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");

        assert_eq!(
            loaded.session_config["codex-acp"].defaults["config:service_tier"],
            "priority"
        );
    }

    #[test]
    fn saved_session_config_merges_server_defaults_with_model_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        let saved = cfg
            .session_config
            .entry("codex-acp".to_string())
            .or_default();
        saved
            .defaults
            .insert("config:service_tier".to_string(), "default".to_string());
        saved
            .models
            .entry("model-a".to_string())
            .or_default()
            .insert("config:service_tier".to_string(), "priority".to_string());
        cfg.save(&path).expect("save");

        assert_eq!(
            load_saved_session_config(&path, "codex-acp", "model-a", SessionConfigSeat::Primary,)["config:service_tier"],
            "priority"
        );
        assert_eq!(
            load_saved_session_config(&path, "codex-acp", "model-b", SessionConfigSeat::Primary,)["config:service_tier"],
            "default"
        );
    }

    #[test]
    fn saved_session_config_keeps_role_defaults_separate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.agent
            .session_defaults
            .entry("codex-acp".to_string())
            .or_default()
            .insert("config:mode".to_string(), "primary".to_string());
        cfg.subagents
            .session_defaults
            .entry("codex-acp".to_string())
            .or_default()
            .insert("config:mode".to_string(), "subagent".to_string());
        cfg.review
            .session_defaults
            .entry("codex-acp".to_string())
            .or_default()
            .insert("config:mode".to_string(), "review".to_string());
        cfg.save(&path).expect("save");

        assert_eq!(
            load_saved_session_config(&path, "codex-acp", "model-a", SessionConfigSeat::Primary,)["config:mode"],
            "primary"
        );
        assert_eq!(
            load_saved_session_config(&path, "codex-acp", "model-a", SessionConfigSeat::Subagent,)
                ["config:mode"],
            "subagent"
        );
        assert_eq!(
            load_saved_session_config(&path, "codex-acp", "model-a", SessionConfigSeat::Review,)["config:mode"],
            "review"
        );
    }

    #[test]
    fn accepted_session_config_is_route_isolated_and_merge_safe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut cfg = Config {
            theme: TerminalThemeKind::Ansi,
            ..Config::default()
        };
        cfg.agent.discrete_review = false;
        cfg.save(&path).expect("save initial config");

        persist_accepted_session_config(
            &path,
            "codex-acp",
            "model-a",
            "config:service_tier".to_string(),
            "priority".to_string(),
        )
        .expect("persist model a");
        persist_accepted_session_config(
            &path,
            "codex-acp",
            "model-b",
            "config:service_tier".to_string(),
            "economy".to_string(),
        )
        .expect("persist model b");

        let loaded = Config::load(&path).expect("load merged config");
        assert_eq!(loaded.theme, TerminalThemeKind::Ansi);
        assert!(!loaded.agent.discrete_review);
        assert_eq!(
            loaded.session_config["codex-acp"].models["model-a"]["config:service_tier"],
            "priority"
        );
        assert_eq!(
            loaded.session_config["codex-acp"].models["model-b"]["config:service_tier"],
            "economy"
        );
    }

    #[test]
    fn settings_save_preserves_a_concurrent_accepted_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut editor_snapshot = Config::default();
        editor_snapshot
            .session_config
            .entry("codex-acp".to_string())
            .or_default()
            .defaults
            .insert("config:service_tier".to_string(), "default".to_string());
        editor_snapshot.save(&path).expect("save editor snapshot");

        persist_accepted_session_config(
            &path,
            "codex-acp",
            "model-a",
            "config:service_tier".to_string(),
            "priority".to_string(),
        )
        .expect("persist accepted route");
        editor_snapshot.theme = TerminalThemeKind::Ansi;
        save_user_config_preserving_session_routes(&path, &mut editor_snapshot)
            .expect("save settings");

        let loaded = Config::load(&path).expect("load merged config");
        assert_eq!(loaded.theme, TerminalThemeKind::Ansi);
        assert_eq!(
            loaded.session_config["codex-acp"].models["model-a"]["config:service_tier"],
            "priority"
        );
    }

    #[test]
    fn changing_a_default_clears_that_key_from_saved_routes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        let saved = config
            .session_config
            .entry("codex-acp".to_string())
            .or_default();
        saved
            .defaults
            .insert("config:service_tier".to_string(), "default".to_string());
        saved
            .models
            .entry("model-a".to_string())
            .or_default()
            .insert("config:service_tier".to_string(), "priority".to_string());
        config.save(&path).expect("save initial config");

        config
            .session_config
            .get_mut("codex-acp")
            .unwrap()
            .defaults
            .insert("config:service_tier".to_string(), "economy".to_string());
        save_user_config_preserving_session_routes(&path, &mut config)
            .expect("save changed default");

        let loaded = Config::load(&path).expect("load config");
        assert!(
            !loaded.session_config["codex-acp"].models["model-a"]
                .contains_key("config:service_tier")
        );
    }

    #[test]
    fn missing_version_discards_old_model_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[models]
thor = "gpt-5-6-sol"
eitri = "gpt-5-6-luna"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_parse_error_is_surfaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"not = valid = toml = @@@").expect("write");
        let err = Config::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"), "error mentions parse: {msg}");
    }

    #[test]
    fn load_expands_home_shortcuts_in_configured_servers() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
version = 3
[[acp.servers]]
id = "custom:my-agent"
label = "my-agent"
command = "~/bin/agent"
args = ["--config", "$HOME/.config/agent.toml"]
origin = "custom"
"#,
        )
        .expect("write");

        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.acp.servers.len(), 1);
        let server = &cfg.acp.servers[0];
        assert_eq!(server.command, home.join("bin/agent"));
        assert_eq!(
            server.args,
            vec![
                "--config".to_string(),
                home.join(".config/agent.toml").display().to_string(),
            ]
        );
    }

    #[test]
    fn configured_acp_servers_validate_ids_commands_and_duplicates() {
        for (body, expected) in [
            (
                "version = 3\n[[acp.servers]]\nid = 'bad name'\nlabel = 'bad'\ncommand = 'server'\norigin = 'custom'\n",
                "unsupported characters",
            ),
            (
                "version = 3\n[[acp.servers]]\nid = 'empty'\nlabel = 'empty'\ncommand = ''\norigin = 'custom'\n",
                "empty command",
            ),
            (
                "version = 3\n[[acp.servers]]\nid = 'same'\nlabel = 'one'\ncommand = 'one'\norigin = 'custom'\n[[acp.servers]]\nid = 'same'\nlabel = 'two'\ncommand = 'two'\norigin = 'custom'\n",
                "duplicate",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("config.toml");
            std::fs::write(&path, body).expect("write");
            let error = Config::load(&path).expect_err("invalid custom server");
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn incompatible_version_is_discarded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
agent = "legacy"
favorite_agents = ["old"]

[scores]
source = "arena"

[session_config.old]
mode = "ask"
"#,
        )
        .expect("write");
        let config = Config::load(&path).expect("load incompatible config");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn server_policies_update_builtins_and_configured_servers() {
        let mut config = Config {
            acp: AcpConfig {
                servers: vec![ConfiguredAcpServer {
                    id: "custom:company".to_string(),
                    label: "company".to_string(),
                    command: PathBuf::from("company-acp"),
                    args: Vec::new(),
                    env: HashMap::new(),
                    origin: AcpServerOrigin::Custom,
                    policy: AcpServerPolicy::Enabled,
                }],
                ..AcpConfig::default()
            },
            ..Config::default()
        };

        assert!(config.set_acp_server_policy("codex-acp", AcpServerPolicy::Disabled));
        assert!(config.set_acp_server_policy("custom:company", AcpServerPolicy::Disabled));
        assert!(!config.set_acp_server_policy("custom:missing", AcpServerPolicy::Disabled));
        assert_eq!(config.acp.policy("codex-acp"), AcpServerPolicy::Disabled);
        assert_eq!(config.acp.servers[0].policy, AcpServerPolicy::Disabled);
    }

    #[test]
    fn team_presets_pin_coder_subagents_and_reviewer() {
        for preset in TeamPreset::ALL {
            let mut config = Config::default();
            config.agent.model = "provider-specific-primary".to_string();
            config.review.model = "provider-specific-review".to_string();
            config.subagents.model = "provider-specific-subagent".to_string();

            preset.apply(&mut config);

            let (coder, reviewer) = preset.sources();
            assert_eq!(TeamPreset::from_config(&config), Some(preset));
            assert_eq!(config.agent.acp_source.as_deref(), Some(coder));
            assert_eq!(config.subagents.acp_source.as_deref(), Some(coder));
            assert_eq!(config.review.acp_source.as_deref(), Some(reviewer));
            assert_eq!(config.agent.model, "auto");
            assert_eq!(config.review.model, "auto");
            assert_eq!(config.subagents.model, "auto");
            assert!(config.agent.discrete_review);
            assert_eq!(config.acp.policy(coder), AcpServerPolicy::Enabled);
            assert_eq!(config.acp.policy(reviewer), AcpServerPolicy::Enabled);
            assert_eq!(TeamPreset::from_id(preset.id()), Some(preset));
        }
    }

    #[test]
    fn default_config_serializes_only_its_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Config::default().save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("version = 3"), "config: {body:?}");
        assert!(
            !body.contains("theme"),
            "default theme should not be serialized: {body:?}"
        );
    }

    #[test]
    fn theme_config_defaulting_and_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write");
        let cfg = Config::load(&path).expect("load default");
        assert_eq!(cfg.theme, TerminalThemeKind::Adaptive);

        let cfg = Config {
            theme: TerminalThemeKind::Ansi,
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("theme = \"ansi\""));

        let loaded = Config::load(&path).expect("load saved");
        assert_eq!(loaded.theme, TerminalThemeKind::Ansi);
    }

    #[test]
    fn spinner_config_defaulting_and_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("write");
        let cfg = Config::load(&path).expect("load default");
        assert_eq!(cfg.spinner, SpinnerStyle::default());

        // Default style is omitted from the serialized form.
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            !body.contains("spinner"),
            "default spinner should not be serialized: {body:?}"
        );

        let cfg = Config {
            spinner: SpinnerStyle::Bars,
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("spinner = \"bars\""));

        let loaded = Config::load(&path).expect("load saved");
        assert_eq!(loaded.spinner, SpinnerStyle::Bars);
    }

    #[test]
    fn thought_output_defaults_to_current_and_full_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(!body.contains("thought_output"));
        assert_eq!(
            Config::load(&path).expect("load default").thought_output,
            ThoughtOutput::Current
        );

        let config = Config {
            thought_output: ThoughtOutput::Full,
            ..Config::default()
        };
        config.save(&path).expect("save full");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("thought_output = \"full\""));
        assert_eq!(
            Config::load(&path).expect("load full").thought_output,
            ThoughtOutput::Full
        );
    }

    #[test]
    fn feature_hints_default_on_and_disabled_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(!body.contains("feature_hints"));

        let config = Config {
            feature_hints: false,
            ..Config::default()
        };
        config.save(&path).expect("save disabled");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("feature_hints = false"));
        assert!(!Config::load(&path).expect("load disabled").feature_hints);
    }

    #[test]
    fn keep_awake_default_on_and_disabled_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Config::default().save(&path).expect("save default");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(!body.contains("keep_awake"));
        assert!(Config::load(&path).expect("load default").keep_awake);

        let config = Config {
            keep_awake: false,
            ..Config::default()
        };
        config.save(&path).expect("save disabled");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("keep_awake = false"));
        assert!(!Config::load(&path).expect("load disabled").keep_awake);
    }
}

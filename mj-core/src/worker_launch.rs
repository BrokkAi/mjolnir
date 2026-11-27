//! Launch descriptions the controller writes and the target-side worker reads.
//!
//! These are plain data types and the constants they name: the file layout of
//! a worker root, the launch configuration for a primary session and for a
//! reviewer child, and how a harness learns about its MCP servers. They
//! carry no process, network, or worker-runtime behaviour, so both sides of
//! the relay can depend on them without depending on each other.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{ExecutionPolicy, HarnessKind};

/// The harness home an installed launch configuration names, or `None` when
/// there is no installed configuration.
///
/// Read loosely, as JSON, because it was written by an earlier release: only
/// the fields that locate the home matter here. A configuration that states no
/// home was started with the harness's home variable.
pub fn installed_harness_home(launch_path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let body = match std::fs::read(launch_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let launch: serde_json::Value = serde_json::from_slice(&body)?;
    Ok(installed_harness_home_from_json(&launch))
}

/// Decode the home from a launch file read on either host.
pub fn installed_harness_home_from_json(launch: &serde_json::Value) -> Option<PathBuf> {
    let stated = launch
        .get("harness_home")
        .and_then(serde_json::Value::as_str)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    if stated.is_some() {
        return stated;
    }
    let harness = launch
        .get("harness")
        .cloned()
        .and_then(|harness| serde_json::from_value::<HarnessKind>(harness).ok())?;
    launch
        .get("environment")
        .and_then(|environment| environment.get(harness.home_env()))
        .and_then(serde_json::Value::as_str)
        .map(|home| harness.home_from_environment(home))
}

/// Where a worker's ACP adapter and harness executable come from.
/// `mj_harness_install::harness_install::locate` turns a policy into a launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessRuntimePolicy {
    /// The target's own installation, for example a container target's image,
    /// with Mjolnir's managed installation when it is missing or incompatible.
    #[default]
    Ambient,
    /// Mjolnir's managed installation of the compiled pin.
    #[serde(rename = "managed_remote", alias = "managed")]
    Managed,
    /// Use harnesses provided by the daemon image. Retained for launch specs
    /// written by older daemons. A missing or incompatible harness is an error.
    Image,
}

impl HarnessRuntimePolicy {
    pub const fn is_ambient(&self) -> bool {
        matches!(self, Self::Ambient)
    }

    /// Whether this source may download Mjolnir's managed harness or Node.js.
    pub const fn may_install(&self) -> bool {
        !matches!(self, Self::Image)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOwnership {
    pub version: u32,
    /// Retain the accepted credential policy if an orphan is adopted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_github_credentials: bool,
    #[serde(default = "default_worker_workspace_id")]
    pub workspace_id: String,
    pub session_id: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_template_id: String,
    /// Instance that created the worker (`config::InstanceIdentity`).
    /// Markers written before version 3 omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

impl WorkerOwnership {
    pub const VERSION: u32 = 3;

    pub fn write(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec(self)?;
        crate::config::atomic_write(path, &body)
    }
}

fn default_worker_workspace_id() -> String {
    crate::workspace::DEFAULT_WORKSPACE_ID.to_owned()
}

fn default_agent_mailboxes_enabled() -> bool {
    true
}

fn default_subtle_subagents() -> bool {
    true
}

fn is_default_subtle_subagents(value: &bool) -> bool {
    *value
}

/// Launch directive that makes the Nitro engine's mbx settings authoritative.
pub const SESSION_NITRO_MBX_ENV: &str = "MJ_SESSION_NITRO_MBX";
pub const SESSION_MBX_BIN_ENV: &str = "MJ_MBX_BIN";
pub const MBX_REMOTE_ENV_PREFIX: &str = "MBX_REMOTE_";
pub const MBX_REMOTE_ENVIRONMENT: [&str; 3] =
    ["MBX_REMOTE_URL", "MBX_REMOTE_NAMESPACE", "MBX_REMOTE_MODE"];

/// Internal launch directive: include this absolute Git config file in the
/// worker-owned session global config, then remove the key before harness use.
pub const SESSION_GIT_CONFIG_INCLUDE_PATH: &str = "MJ_SESSION_GIT_CONFIG_INCLUDE_PATH";
/// Controller-authored environment marker so old launch.json readers remain
/// compatible while a new worker can distinguish a managed child whose
/// historical handback tool is disabled.
pub const SESSION_MANAGED_SUBAGENT_ENV: &str = "MJ_SESSION_MANAGED_SUBAGENT";
/// Controller-authored environment marker: record a raw Git launch baseline
/// (`mj/sessions/<session-id>/launch`) before starting the harness if absent.
pub const SESSION_RAW_LAUNCH_RECORD_ENV: &str = "MJ_SESSION_RAW_LAUNCH_RECORD";
/// Controller-authored environment marker: the controller staged the
/// message-only `mj-agents` registration for this top-level session. A worker
/// grants that role only with this marker, so a newer worker resuming a launch
/// written by an older controller does not demand a registration that was
/// never staged.
pub const SESSION_MESSAGE_MCP_ENV: &str = "MJ_SESSION_MESSAGE_MCP";

/// Controller-authored JSON checkout roots for container-only project tool
/// installation. The worker consumes this directive before starting a harness.
pub const SESSION_PROJECT_TOOLS_ROOTS: &str = "MJ_SESSION_PROJECT_TOOLS_ROOTS";

/// Absolute path of this session's scratch directory,
/// `<checkout>/.mj/keep/<session-id>/`. Move copies that directory; suspend
/// does not save it. The worker sets it after creating the directory.
pub const SESSION_KEEP_DIR_ENV: &str = "MJ_KEEP_DIR";

/// Whether this worker executes the harness or only preserves recovered state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRunMode {
    #[default]
    Harness,
    CheckpointOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLaunchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_move: Option<ParkedMoveLaunch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_reload_operation_id: Option<String>,
    /// Chosen from the target locator, never from ambient container detection.
    #[serde(
        default,
        skip_serializing_if = "crate::cpu_usage::CpuAccountingMode::is_process_tree"
    )]
    pub cpu_accounting: crate::cpu_usage::CpuAccountingMode,
    /// Accepted session policy; do not seed or accept GitHub authentication.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_github_credentials: bool,
    /// Ordered input journaled before this worker accepts external commands.
    /// Kept in the launch description so a crash during initialization can
    /// finish the same prefix under its original command IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_commands: Vec<InitialWorkerCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_launch: Option<ReviewerLaunchConfig>,
    /// Unique intent for a user-requested Resume/Restart; recovery preserves it in the relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_resume_request: Option<String>,
    #[serde(default)]
    pub run_mode: WorkerRunMode,
    /// Writer proof for the accepted suspend, performed before checkpoint recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_capture: Option<crate::worker_suspend::SuspendCapture>,
    pub session_id: String,
    /// Resolved delegation policy; independently controls MCP exposure and native suppression.
    #[serde(
        default,
        alias = "subagent_tools",
        deserialize_with = "crate::subagent::deserialize_launch_policy"
    )]
    pub subagents: crate::subagent::SubagentPolicy,
    /// Whether this is a sub-agent child that hands its report back through
    /// the `mj-agents` MCP server's `handback` tool. Children suppress native delegation.
    #[serde(default)]
    pub handback_tool: bool,
    /// Explicit child identity; older launch files deliver handbacks unchecked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_kind: Option<crate::subagent::SubagentKind>,
    /// Whether this child is a depth-1 orchestrator.
    #[serde(
        rename = "supervisor",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub orchestrator: bool,
    /// Whether this top-level session may spawn depth-1 orchestrators.
    #[serde(
        rename = "supervisors_enabled",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub orchestrators_enabled: bool,
    /// Legacy wire flag retained for older workers. New workers ignore it.
    /// Mailbox delivery and hooks are always present in protocol-41 workers.
    #[serde(default = "default_agent_mailboxes_enabled")]
    pub agent_mailboxes_enabled: bool,
    /// Whether a delegating parent may choose the subtle task type. False when
    /// only the straightforward type is configured: the spawn tool then has no
    /// `task_type` and every child uses the straightforward choice. Launch
    /// files written before this field offered both types.
    #[serde(
        default = "default_subtle_subagents",
        skip_serializing_if = "is_default_subtle_subagents"
    )]
    pub subtle_subagents: bool,
    /// Whether the worker's `mj-agents` server lists `ask_oracle`. Written
    /// only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub oracle: bool,
    /// The model this session was created for, when its creator chose one: a
    /// sub-agent's spawn model. The harness opens a new session on it rather
    /// than on the profile default; once the session has accepted a model,
    /// that one wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_model: Option<String>,
    /// Accepted from older daemon launch files; no longer used or written.
    #[serde(default, skip_serializing)]
    pub review_capture: bool,
    #[serde(default)]
    pub review_trigger: crate::config::ReviewTrigger,
    /// Explicit target settings shared by primary and reviewer processes.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub target_environment: std::collections::BTreeMap<String, String>,
    /// Restore the container image's declared `ENV` onto the discovered login
    /// environment before applying `target_environment`. True only for
    /// container targets, where the worker's own environment is the image's;
    /// on a bare or localhost target the ambient environment is the user's
    /// shell and must not be carried.
    #[serde(default)]
    pub seed_image_environment: bool,
    pub harness: HarnessKind,
    /// The harness home this session reads and writes inside the target.
    ///
    /// The worker locates credentials and skills through it, so it is stated
    /// rather than read back out of `environment`: a harness whose home cannot
    /// be scoped by environment sets no home variable at all. Configs
    /// persisted by older releases omit the field; the worker then falls back
    /// to the harness's home variable, which those releases always set.
    #[serde(default)]
    pub harness_home: PathBuf,
    /// Path, relative to the staged harness home, of the file that proves
    /// authentication, such as `auth.json` or `credentials/kimi-code.json`.
    /// An API-key profile is proven by its harness configuration file rather
    /// than a credential file, so the controller decides the name and the
    /// worker does not re-derive it. Configs persisted by older releases omit
    /// the field; the worker then falls back to the harness kind's own marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication_marker: Option<String>,
    pub bridge_command: PathBuf,
    pub bridge_args: Vec<String>,
    #[serde(default, skip_serializing_if = "HarnessRuntimePolicy::is_ambient")]
    pub harness_runtime: HarnessRuntimePolicy,
    pub environment: std::collections::BTreeMap<String, String>,
    /// Variables the worker removes from the harness environment after it adds
    /// the target's own login environment, so that a value set on the target
    /// (a shell profile, the image, the target's settings) cannot reach the
    /// harness either. The controller has already left them out of
    /// `environment`. See
    /// [`crate::config::HarnessProfile::exclude_harness_environment`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_environment: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
    /// Ancestor and owner workspace roots that stay writable when a borrowed
    /// child's `working_directory` moves `cwd` below its owner's checkout. The
    /// harness sandbox and ACP session grant them, but they are kept apart from
    /// `additional_directories` so multi-repository review discovery and
    /// Muse's single-repository check see only the repositories the session
    /// was launched with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_directories: Vec<PathBuf>,
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_memory: Option<ProjectMemoryLaunchConfig>,
    /// Target-level policy translated into harness-specific controls by the
    /// worker. Localhost and guardian SSH targets preserve configured
    /// approvals for harnesses that support them. Other targets run
    /// unconstrained.
    #[serde(
        alias = "force_unrestricted_mode",
        deserialize_with = "deserialize_execution_policy"
    )]
    pub execution_policy: ExecutionPolicy,
    /// The Claude profile's `claude_permission_mode`, which picks Claude's
    /// mode under configured approvals. Written only when it is not Auto, so
    /// launches that keep the default stay readable by older workers.
    #[serde(
        default,
        skip_serializing_if = "crate::config::ClaudePermissionMode::is_auto"
    )]
    pub claude_permission_mode: crate::config::ClaudePermissionMode,
    /// The profile's `mjolnir_guardian`, which decides whether the worker
    /// reviews a Kimi Code, OpenCode, or DSH session's approvals under
    /// configured approvals. Written only when off, so launches that keep the
    /// default stay readable by older workers.
    #[serde(
        default = "crate::config::default_true",
        skip_serializing_if = "crate::config::is_true"
    )]
    pub mjolnir_guardian: bool,
    /// Accepted from older daemon launch files, which carried Brokk Observe
    /// guardrail settings here; no longer used or written. This struct
    /// rejects unknown fields, so dropping the name would make a new worker
    /// refuse those launches. The contents (including a bearer token) are
    /// discarded while parsing, so they never reach `Debug` output.
    #[serde(default, skip_serializing)]
    pub observe_policy: Option<serde::de::IgnoredAny>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkedMoveLaunch {
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discard_native_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_context: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discard_queued_prompts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_continuation: Option<InitialWorkerCommand>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialWorkerCommand {
    pub command_id: String,
    pub command: crate::relay::RelayCommand,
    /// Optional configuration may be refused without abandoning the brief.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

fn deserialize_execution_policy<'de, D>(deserializer: D) -> Result<ExecutionPolicy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum WirePolicy {
        Current(ExecutionPolicy),
        Legacy(bool),
    }

    Ok(match WirePolicy::deserialize(deserializer)? {
        WirePolicy::Current(policy) => policy,
        WirePolicy::Legacy(true) => ExecutionPolicy::Unconstrained,
        WirePolicy::Legacy(false) => ExecutionPolicy::ConfiguredApprovals,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemoryLaunchConfig {
    /// Private worker control socket for controller-backed history tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_socket: Option<PathBuf>,
    /// Stable controller-derived identity for this repository or bundle.
    pub project_key: String,
    /// Target-side replica for file-based project notes and controller sync.
    pub root: PathBuf,
    /// Session-private copy of the canonical tree from the last successful
    /// synchronization, used as the three-way merge base.
    #[serde(default)]
    pub baseline_root: PathBuf,
    /// Bundle repository IDs mapped to the roots presented over ACP.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub repository_roots: std::collections::BTreeMap<String, PathBuf>,
    /// How the harness learns about the project-history MCP server. Most ACP
    /// adapters accept a stdio server in `session/new`; adapters that need
    /// harness-specific runtime metadata receive it through their staged
    /// profile instead.
    #[serde(default, skip_serializing_if = "ProjectMemoryMcpDelivery::is_acp")]
    pub mcp_delivery: ProjectMemoryMcpDelivery,
}

/// The session's seed is a sibling of its baseline and never part of a profile overlay.
pub fn project_memory_seed_root(baseline_root: &Path) -> PathBuf {
    baseline_root.with_file_name(".hel-memory-seed")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMemoryMcpDelivery {
    #[default]
    Acp,
    HarnessProfile,
}

impl ProjectMemoryMcpDelivery {
    fn is_acp(&self) -> bool {
        *self == Self::Acp
    }
}

/// The output contract a review child receives from its MCP server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewChildRole {
    #[default]
    Reviewer,
    BriefWriter,
}

impl ReviewChildRole {
    fn is_reviewer(&self) -> bool {
        *self == Self::Reviewer
    }
}

/// Harness launch settings for a reviewer child.
///
/// The reviewer shares the primary's target and working directory and nothing
/// else: its harness home is a fresh copy of the chosen profile, staged under
/// its own worker root, and the worker sets that home itself so a
/// controller can never point a reviewer at the primary's credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerLaunchConfig {
    /// Omitted for reviewers so older workers keep reading their launches.
    /// Older workers reject a writer's unknown field before starting a harness.
    #[serde(default, skip_serializing_if = "ReviewChildRole::is_reviewer")]
    pub role: ReviewChildRole,
    /// Configured profile this reviewer was staged from, for display and for
    /// deciding whether a saved reviewer still matches the user's choice.
    pub profile_id: String,
    pub harness: HarnessKind,
    pub bridge_command: PathBuf,
    pub bridge_args: Vec<String>,
    /// Read only, for records written before the launch took the profile's
    /// environment from the live profile. It is never written again: the
    /// values were resolved secrets that the record outlived.
    #[serde(default, skip_serializing)]
    pub environment: std::collections::BTreeMap<String, String>,
    /// Variables the worker removes from the reviewer's environment after it
    /// adds the target's login environment; see
    /// [`WorkerLaunchConfig::excluded_environment`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_environment: Vec<String>,
    pub execution_policy: ExecutionPolicy,
    /// Model to apply once the session opens, or `None` to keep the profile's
    /// default. Explicit selections must be supported by the target adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Best-effort fast mode, applied only when the harness advertises it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    /// Legacy launch field retained so saved worker configurations remain readable.
    #[serde(default)]
    pub generation: u64,
    /// Additional configured MCP servers this reviewer gets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<ReviewMcpServer>,
}

/// One stdio MCP server a reviewing agent is given.
///
/// How it reaches the harness depends on the harness: most accept a server in
/// the ACP `session/new` request, while Claude and Kimi read their own
/// configuration files, which the controller patches while staging the
/// reviewer's profile. [`ReviewMcpDelivery`] is the single place that decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewMcpServer {
    pub name: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

/// How a harness learns about a reviewing agent's MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMcpDelivery {
    /// Attached to the ACP `session/new` request.
    Acp,
    /// Written into the staged profile the harness reads at startup.
    HarnessProfile,
}

impl ReviewMcpDelivery {
    /// Claude and Kimi both ignore servers offered over ACP -- Claude is not
    /// given them at all (see `project_history_mcp` in `src/acp.rs`), and
    /// Kimi needs runtime metadata its own schema carries -- so both are
    /// configured through their staged profile instead.
    #[must_use]
    pub const fn for_harness(harness: HarnessKind) -> Self {
        match harness {
            HarnessKind::Claude | HarnessKind::Kimi => Self::HarnessProfile,
            _ => Self::Acp,
        }
    }
}

/// Resolve the `mj-agents` server role from the launch values that control
/// whether its durable request queue exists in the worker.
#[must_use]
pub fn resolve_subagent_mcp_role(
    subagents: &crate::subagent::SubagentPolicy,
    managed_subagent: bool,
    orchestrator: bool,
    handback_tool: bool,
    _legacy_agent_mailboxes_enabled: bool,
    message_mcp_granted: bool,
) -> Option<crate::subagent::SubagentMcpRole> {
    if managed_subagent && orchestrator {
        return Some(crate::subagent::SubagentMcpRole::Orchestrator);
    }
    subagents.parent_role().or_else(|| {
        if managed_subagent {
            Some(if handback_tool {
                crate::subagent::SubagentMcpRole::Child
            } else {
                crate::subagent::SubagentMcpRole::FetchOnly
            })
        } else {
            message_mcp_granted.then_some(crate::subagent::SubagentMcpRole::MessageOnly)
        }
    })
}

impl WorkerLaunchConfig {
    /// The profile's approval choices the controller resolved into this
    /// launch.
    pub fn approval_choices(&self) -> crate::config::ApprovalChoices {
        crate::config::ApprovalChoices {
            claude_permission_mode: self.claude_permission_mode,
            mjolnir_guardian: self.mjolnir_guardian,
        }
    }

    /// Whether this session is offered the `mj-review` tools. The controller
    /// enables `review_trigger` for workspace owners with push review.
    pub fn review_tools(&self) -> bool {
        offers_review_tools(self.review_trigger, self.harness)
    }

    /// Whether launch must prepare review state in the workspace: the session
    /// start pin for every session offered the review tools, and the push gate
    /// (or a notice that this harness cannot enforce one) for a primary.
    pub fn prepares_review(&self) -> bool {
        self.review_trigger == crate::config::ReviewTrigger::Push && self.reviewer_launch.is_none()
    }

    /// Whether pushes from this session are held to review: the pre-push
    /// hook, `MJ_REVIEW_GATE` and the session-start pin.
    pub fn review_gate(&self) -> bool {
        self.review_tools() && self.reviewer_launch.is_none()
    }

    /// Managed ownership is independent of the MCP role and historical handback tools.
    pub fn is_managed_subagent(&self) -> bool {
        self.environment
            .get(SESSION_MANAGED_SUBAGENT_ENV)
            .is_some_and(|value| value == "1")
    }

    /// The `mj-agents` role is derived once from the launch's ownership and
    /// policy so Codex and Claude expose the same tools.
    pub fn subagent_mcp_role(&self) -> Option<crate::subagent::SubagentMcpRole> {
        resolve_subagent_mcp_role(
            &self.subagents,
            self.is_managed_subagent(),
            self.orchestrator,
            self.handback_tool,
            self.agent_mailboxes_enabled,
            self.environment
                .get(SESSION_MESSAGE_MCP_ENV)
                .is_some_and(|value| value == "1")
                || self.review_gate(),
        )
    }

    /// Downloads must finish before startup or an upgrade's idle reservation.
    pub fn requires_harness_preparation(&self) -> bool {
        self.harness_runtime == HarnessRuntimePolicy::Managed
            || crate::harness_runtime::npm_bridge(self.harness).is_some_and(|bridge| {
                bridge.matches_launcher(&self.bridge_command, &self.bridge_args)
            })
            || crate::harness_runtime::is_fallback_launch(
                self.harness,
                self.execution_policy,
                &self.bridge_command,
                &self.bridge_args,
            )
    }

    pub fn read(path: &Path) -> Result<Self> {
        let body = std::fs::read(path)
            .with_context(|| format!("read worker launch config {}", path.display()))?;
        let parse = || -> serde_json::Result<Self> {
            let mut value: serde_json::Value = serde_json::from_slice(&body)?;
            // Shipped workers keep launch.json across upgrades. The removed
            // runtime constraint and turn review's former Bifrost binary have
            // no effect, but must not block loading that file.
            if let Some(fields) = value.as_object_mut() {
                fields.remove("expected_runtime_identity");
                fields.remove("bifrost_binary");
            }
            serde_json::from_value(value)
        };
        let mut launch: Self = parse().with_context(|| {
            format!("parse worker launch config {} with worker build {}", path.display(), crate::worker_build::worker_build_id())
        }).map_err(|error| {
            if error.root_cause().to_string().contains("unknown field") {
                error.context("the worker may be older than the daemon that wrote launch.json; install the worker built with the running mj")
            } else {
                error
            }
        })?;
        // Daemons from 2.11 through 2.23 stamped their instance identity here.
        // For the default instance that is a data-directory fingerprint, which
        // every `mj` in the session then read as a named instance. Workers
        // launched then keep that config across upgrades, so drop it on load.
        launch
            .target_environment
            .remove(crate::config::INSTANCE_ENV);
        if launch
            .target_environment
            .contains_key(SESSION_NITRO_MBX_ENV)
        {
            // Older launch files may contain remote options from the target
            // or profile. The Nitro engine supplies them in the process env.
            launch
                .target_environment
                .retain(|name, _| !name.starts_with(MBX_REMOTE_ENV_PREFIX));
            launch
                .environment
                .retain(|name, _| !name.starts_with(MBX_REMOTE_ENV_PREFIX));
        }
        Ok(launch)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let body = serde_json::to_vec_pretty(self)?;
        use std::io::Write;
        let temporary = parent.join(format!(
            ".launch-{}.tmp",
            crate::relay::new_command_id("write")?
        ));
        let mut created = false;
        let result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            created = true;
            file.write_all(&body)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            crate::config::sync_directory(parent)?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        Ok(())
    }
}

/// Content address of a worker executable.
///
/// One definition, because both sides of the upgrade decision compare it: the
/// worker reports the digest of the file serving it, and the controller
/// computes the digest of the file it would install. The digest and build
/// stamp share a streaming read and are memoized by executable identity on Unix.
pub fn worker_executable_digest(path: &Path) -> Result<String> {
    crate::worker_build::executable_digest(path)
}

/// Content address of the executable running this process, or `None` when it
/// cannot be read.
///
/// Read through `/proc/self/exe` on Linux: a worker whose file was replaced
/// under it - which is exactly what an upgrade does - still has its own image
/// there, while the resolved path no longer names it.
pub fn running_executable_digest() -> Option<String> {
    let path = if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, "could not resolve this executable to report its build");
                return None;
            }
        }
    };
    match worker_executable_digest(&path) {
        Ok(digest) => Some(digest),
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not hash this executable to report its build"
            );
            None
        }
    }
}

/// Private, prompt-free harness discovery. All paths belong to one probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileProbeSpec {
    pub harness: HarnessKind,
    pub profile_home: PathBuf,
    pub environment: std::collections::BTreeMap<String, String>,
    /// Variables the probe removes from the harness environment after it adds
    /// the login environment; see
    /// [`WorkerLaunchConfig::excluded_environment`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_environment: Vec<String>,
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Where the probe's harness comes from: the same source as a local
    /// session ([`crate::placement::local_harness_runtime`]). Specs written by
    /// older daemons omit it and always probed the managed installation.
    #[serde(default = "managed_harness_runtime")]
    pub harness_runtime: HarnessRuntimePolicy,
}

fn managed_harness_runtime() -> HarnessRuntimePolicy {
    HarnessRuntimePolicy::Managed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub model: Option<String>,
    pub models: Vec<crate::acp::SessionConfigChoice>,
    pub efforts: Vec<crate::acp::SessionConfigChoice>,
    pub observed_at: i64,
}

impl ProfileConfig {
    /// The catalogue as Mjolnir offers it under a profile's [`ModelOffer`]:
    /// the listed models outside excluded scopes, plus the profile's current
    /// model, which stays so a session never runs a model its picker hides.
    /// An empty list with no excluded scope offers everything.
    ///
    /// [`ModelOffer`]: crate::config::ModelOffer
    pub fn restricted_to(mut self, offer: &crate::config::ModelOffer) -> Self {
        if !offer.is_unrestricted() {
            let current = self.model.clone();
            self.models.retain(|choice| {
                offer.allows(&choice.value) || current.as_ref() == Some(&choice.value)
            });
        }
        self
    }
}

/// Whether a session with this review trigger on this harness is offered the
/// mj-review tools. Launch and the daemon's completion checks share this one
/// rule.
pub fn offers_review_tools(
    trigger: crate::config::ReviewTrigger,
    harness: crate::config::HarnessKind,
) -> bool {
    trigger == crate::config::ReviewTrigger::Push && harness.supports_review_gate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn worker_launch_write_atomically_replaces_private_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launch.json");
        std::fs::write(&path, "old launch").unwrap();
        let mut old = std::fs::File::open(&path).unwrap();
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        launch.write(&path).unwrap();
        assert_ne!(
            old.metadata().unwrap().ino(),
            path.metadata().unwrap().ino()
        );
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let mut previous = String::new();
        std::io::Read::read_to_string(&mut old, &mut previous).unwrap();
        assert_eq!(previous, "old launch");
        assert!(WorkerLaunchConfig::read(&path).is_ok());
        let blocked = dir.path().join("directory");
        std::fs::create_dir(&blocked).unwrap();
        assert!(launch.write(&blocked).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn legacy_cpu_wire_and_launch_default_to_process_tree() {
        use crate::cpu_usage::{CpuAccountingMode, CpuSampleScope, SessionCpuUsage};
        let legacy = serde_json::json!({
            "recent_permille": 10, "hourly_permille": 20,
            "hourly_covered_secs": 30, "online_cpus": 4
        });
        let usage: SessionCpuUsage = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(usage.scope, CpuSampleScope::ProcessTree);
        let mut container = legacy;
        container["scope"] = serde_json::json!("container");
        assert_eq!(
            serde_json::from_value::<SessionCpuUsage>(container)
                .unwrap()
                .scope,
            CpuSampleScope::Container
        );
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        assert_eq!(launch.cpu_accounting, CpuAccountingMode::ProcessTree);
        assert!(
            serde_json::to_value(&launch)
                .unwrap()
                .get("cpu_accounting")
                .is_none()
        );
        let mut launch = launch;
        launch.cpu_accounting = CpuAccountingMode::Container;
        let saved = serde_json::to_value(launch).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkerLaunchConfig>(saved)
                .unwrap()
                .cpu_accounting,
            CpuAccountingMode::Container
        );
    }

    fn launch_json() -> serde_json::Value {
        serde_json::json!({
            "session_id": "session",
            "harness": "codex",
            "bridge_command": "codex-acp",
            "bridge_args": [],
            "environment": {},
            "cwd": "/workspace/project",
            "execution_policy": "configured_approvals"
        })
    }

    #[test]
    fn worker_launch_renamed_orchestrator_fields_keep_the_strict_legacy_wire_shape() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RetainedWorkerFields {
            #[serde(default)]
            supervisor: bool,
            #[serde(default)]
            supervisors_enabled: bool,
        }

        let mut launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        let baseline = serde_json::to_value(&launch).unwrap();
        assert!(baseline.get("suspend_capture").is_none());
        for (orchestrator, enabled) in [(true, false), (false, true)] {
            launch.orchestrator = orchestrator;
            launch.orchestrators_enabled = enabled;
            let payload = serde_json::to_value(&launch).unwrap();
            let fields: serde_json::Map<_, _> = payload
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, value)| baseline.get(*key) != Some(*value))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let retained: RetainedWorkerFields = serde_json::from_value(fields.into()).unwrap();
            assert_eq!(retained.supervisor, orchestrator);
            assert_eq!(retained.supervisors_enabled, enabled);

            let decoded: WorkerLaunchConfig = serde_json::from_value(payload).unwrap();
            assert_eq!(decoded.orchestrator, orchestrator);
            assert_eq!(decoded.orchestrators_enabled, enabled);
        }
    }

    /// A launch file written before per-turn review was removed still names
    /// `"turn"`. It must keep parsing, and reads as the push gate.
    #[test]
    fn a_launch_file_written_for_turn_review_still_parses_as_push() {
        let mut json = launch_json();
        json["review_trigger"] = serde_json::json!("turn");
        json["review_capture"] = serde_json::json!(true);
        let launch: WorkerLaunchConfig = serde_json::from_value(json).unwrap();
        assert_eq!(launch.review_trigger, crate::config::ReviewTrigger::Push);
        assert!(launch.review_gate());
        let rewritten = serde_json::to_value(&launch).unwrap();
        assert_eq!(rewritten["review_trigger"], "push");
        assert!(rewritten.get("review_capture").is_none());
    }

    // Hard-won: 32d96d199: push reviews exposed delegation tools with subagents=None.
    #[test]
    fn persisted_review_trigger_controls_primary_mcp_without_leaking_to_children() {
        use crate::subagent::{SubagentMcpRole, SubagentPolicy};

        for harness in [
            HarnessKind::Claude,
            HarnessKind::Codex,
            HarnessKind::Dsh,
            HarnessKind::Kimi,
            HarnessKind::Muse,
            // Delegates through Mjolnir but has no push review gate.
            HarnessKind::OpenCode,
        ] {
            for trigger in [
                crate::config::ReviewTrigger::Off,
                crate::config::ReviewTrigger::Push,
            ] {
                let mut json = launch_json();
                json["harness"] = serde_json::to_value(harness).unwrap();
                json["review_trigger"] = serde_json::to_value(trigger).unwrap();
                let mut launch: WorkerLaunchConfig = serde_json::from_value(json).unwrap();
                let saved = serde_json::to_value(&launch).unwrap();
                let read: WorkerLaunchConfig = serde_json::from_value(saved).unwrap();
                assert_eq!(read.review_trigger, trigger);
                let expected = trigger == crate::config::ReviewTrigger::Push
                    && !matches!(harness, HarnessKind::Muse | HarnessKind::OpenCode);
                assert_eq!(launch.review_gate(), expected);
                for policy in [
                    SubagentPolicy::None,
                    SubagentPolicy::Native,
                    SubagentPolicy::Mjolnir {
                        orchestrators: false,
                        straightforward: None,
                        subtle: None,
                    },
                ] {
                    launch.subagents = policy.clone();
                    let saved = serde_json::to_value(&launch).unwrap();
                    let read: WorkerLaunchConfig = serde_json::from_value(saved).unwrap();
                    let expected_role = match policy {
                        SubagentPolicy::Mjolnir { .. } => Some(SubagentMcpRole::Parent),
                        _ if expected => Some(SubagentMcpRole::MessageOnly),
                        _ => None,
                    };
                    assert_eq!(read.subagent_mcp_role(), expected_role);
                }
                launch.subagents = SubagentPolicy::None;
                launch
                    .environment
                    .insert(SESSION_MANAGED_SUBAGENT_ENV.into(), "1".into());
                // Managed identity does not suppress an owning launch's gate.
                assert_eq!(launch.review_gate(), expected);
                assert_eq!(
                    launch.subagent_mcp_role(),
                    Some(crate::subagent::SubagentMcpRole::FetchOnly)
                );
                launch.handback_tool = true;
                assert_eq!(
                    launch.subagent_mcp_role(),
                    Some(crate::subagent::SubagentMcpRole::Child)
                );
            }
        }
    }

    #[test]
    fn unknown_launch_fields_explain_the_worker_daemon_build_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launch.json");
        let mut launch = launch_json();
        launch["future_daemon_field"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_vec(&launch).unwrap()).unwrap();
        let error = format!("{:#}", WorkerLaunchConfig::read(&path).unwrap_err());
        assert!(error.contains("older than the daemon"), "{error}");
        assert!(error.contains("future_daemon_field"), "{error}");
        assert!(
            error.contains(crate::worker_build::worker_build_id()),
            "{error}"
        );
        std::fs::write(&path, b"not JSON").unwrap();
        let error = format!("{:#}", WorkerLaunchConfig::read(&path).unwrap_err());
        assert!(!error.contains("older than the daemon"), "{error}");
    }

    // Hard-won: 5fc728617: a brief writer must never silently receive the reviewer contract.
    #[test]
    fn reviewer_launch_roles_keep_old_reviewers_readable_and_reject_unknown_contracts() {
        let mut value = serde_json::json!({
            "profile_id":"reviewer", "harness":"codex", "bridge_command":"bridge",
            "bridge_args":[], "execution_policy":"configured_approvals"
        });
        // Build from an existing launch fixture's policy representation.
        value["execution_policy"] =
            serde_json::to_value(ExecutionPolicy::ConfiguredApprovals).unwrap();
        let reviewer: ReviewerLaunchConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(reviewer.role, ReviewChildRole::Reviewer);
        assert!(
            serde_json::to_value(&reviewer)
                .unwrap()
                .get("role")
                .is_none()
        );
        value["role"] = serde_json::json!("brief_writer");
        let writer: ReviewerLaunchConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(writer.role, ReviewChildRole::BriefWriter);
        assert_eq!(
            serde_json::to_value(writer).unwrap()["role"],
            "brief_writer"
        );
        value["role"] = serde_json::json!("future_role");
        assert!(serde_json::from_value::<ReviewerLaunchConfig>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove("role");
        value["future_role"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<ReviewerLaunchConfig>(value)
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn saved_launch_drops_the_retired_runtime_constraint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launch.json");
        let mut value = launch_json();
        value["expected_runtime_identity"] = serde_json::json!("mj-runtime-v1:retired");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let launch = WorkerLaunchConfig::read(&path).unwrap();
        assert_eq!(launch.session_id, "session");
        assert_eq!(launch.bridge_command, Path::new("codex-acp"));
        launch.write(&path).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("expected_runtime_identity").is_none());
    }

    #[test]
    fn saved_launch_drops_the_retired_bifrost_binary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("launch.json");
        let mut value = launch_json();
        value["bifrost_binary"] = serde_json::json!("/usr/local/bin/bifrost");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let launch = WorkerLaunchConfig::read(&path).unwrap();
        launch.write(&path).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("bifrost_binary").is_none());
    }

    #[test]
    fn old_launch_configs_default_to_ambient_harnesses() {
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Ambient);
        assert!(launch.agent_mailboxes_enabled);

        let mut disabled = launch_json();
        disabled["agent_mailboxes_enabled"] = serde_json::json!(false);
        let launch: WorkerLaunchConfig = serde_json::from_value(disabled).unwrap();
        assert!(!launch.agent_mailboxes_enabled);
    }

    #[test]
    fn built_in_container_launchers_are_prepared_before_start() {
        for harness in HarnessKind::ALL {
            let (command, args) =
                crate::harness_runtime::fallback_launch(harness, ExecutionPolicy::Unconstrained);
            let mut value = launch_json();
            value["harness"] = serde_json::to_value(harness).unwrap();
            value["bridge_command"] = serde_json::to_value(command).unwrap();
            value["bridge_args"] = serde_json::to_value(args).unwrap();
            value["execution_policy"] = serde_json::json!("unconstrained");
            let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
            assert!(
                launch.requires_harness_preparation(),
                "{} must decide between its target launcher and managed installation",
                harness.display_name()
            );
        }

        let mut custom = launch_json();
        custom["bridge_command"] = serde_json::json!("/opt/custom-agent");
        let custom: WorkerLaunchConfig = serde_json::from_value(custom).unwrap();
        assert!(!custom.requires_harness_preparation());
    }

    /// Older controllers required both task types, so a launch without the
    /// field offers subtle. Only the straightforward-only case is written, so
    /// a launch offering both stays readable by an older worker.
    #[test]
    fn subtle_subagents_defaults_on_and_is_written_only_when_off() {
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        assert!(launch.subtle_subagents);
        assert!(
            serde_json::to_value(&launch)
                .unwrap()
                .get("subtle_subagents")
                .is_none()
        );

        let mut only_straightforward = launch_json();
        only_straightforward["subtle_subagents"] = serde_json::json!(false);
        let launch: WorkerLaunchConfig = serde_json::from_value(only_straightforward).unwrap();
        assert!(!launch.subtle_subagents);
        assert_eq!(
            serde_json::to_value(&launch).unwrap()["subtle_subagents"],
            false
        );
    }

    #[test]
    fn oracle_defaults_off_and_is_written_only_when_on() {
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        assert!(!launch.oracle);
        assert!(launch.subagent_kind.is_none());
        assert!(
            serde_json::to_value(&launch)
                .unwrap()
                .get("subagent_kind")
                .is_none()
        );
        for kind in [
            crate::subagent::SubagentKind::Worker,
            crate::subagent::SubagentKind::Reviewer,
            crate::subagent::SubagentKind::Oracle,
        ] {
            let mut wire = launch_json();
            wire["subagent_kind"] = serde_json::to_value(kind).unwrap();
            let decoded: WorkerLaunchConfig = serde_json::from_value(wire).unwrap();
            assert_eq!(decoded.subagent_kind, Some(kind));
            assert_eq!(
                serde_json::to_value(decoded).unwrap()["subagent_kind"],
                serde_json::to_value(kind).unwrap()
            );
        }
        assert!(
            serde_json::to_value(&launch)
                .unwrap()
                .get("oracle")
                .is_none()
        );
        let mut on = launch_json();
        on["oracle"] = serde_json::json!(true);
        let launch: WorkerLaunchConfig = serde_json::from_value(on).unwrap();
        assert!(launch.oracle);
        assert_eq!(serde_json::to_value(&launch).unwrap()["oracle"], true);
    }

    // Hard-won: f9a3130c: upgraded workers on older launches demanded an unstaged Claude registration.
    #[test]
    fn a_launch_from_an_older_controller_gets_no_message_only_server() {
        // An older controller staged no `mj-agents` server for a top-level
        // session and wrote no marker; mailboxes alone must not imply one.
        let launch: WorkerLaunchConfig = serde_json::from_value(launch_json()).unwrap();
        assert!(launch.agent_mailboxes_enabled);
        assert_eq!(launch.subagent_mcp_role(), None);

        let mut staged = launch_json();
        staged["environment"][SESSION_MESSAGE_MCP_ENV] = serde_json::json!("1");
        let launch: WorkerLaunchConfig = serde_json::from_value(staged).unwrap();
        assert_eq!(
            launch.subagent_mcp_role(),
            Some(crate::subagent::SubagentMcpRole::MessageOnly)
        );
    }

    #[test]
    fn a_launch_from_a_daemon_with_observe_guardrails_still_loads_and_drops_them() {
        // Older daemons wrote Brokk Observe settings under `observe_policy`.
        // The struct rejects unknown fields, so the name must stay accepted.
        let mut value = launch_json();
        value["observe_policy"] = serde_json::json!({
            "url": "http://observe.invalid", "token": "secret", "timeout_ms": 3000, "fail_closed": true
        });
        let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
        assert!(launch.observe_policy.is_some());
        assert!(
            serde_json::to_value(&launch)
                .unwrap()
                .get("observe_policy")
                .is_none()
        );
    }

    #[test]
    fn managed_policy_accepts_new_and_legacy_wire_names() {
        let mut value = launch_json();
        value["harness_runtime"] = serde_json::json!("managed_remote");
        let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Managed);
        assert_eq!(
            serde_json::to_value(&launch).unwrap()["harness_runtime"],
            "managed_remote"
        );

        let mut value = launch_json();
        value["harness_runtime"] = serde_json::json!("managed");
        let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
        assert_eq!(launch.harness_runtime, HarnessRuntimePolicy::Managed);
    }
    #[test]
    fn launch_policy_accepts_legacy_selectors_and_booleans() {
        use crate::subagent::SubagentPolicy;
        for (policy, expected) in [
            (
                serde_json::json!({"mode":"mjolnir", "model":"gpt-6-luna", "effort":"max"}),
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: Some(Box::new(crate::config::SubagentChoice {
                        model: "gpt-6-luna".into(),
                        effort: Some("max".into()),
                    })),
                    subtle: Some(Box::new(crate::config::SubagentChoice {
                        model: "gpt-6-luna".into(),
                        effort: Some("max".into()),
                    })),
                },
            ),
            (
                serde_json::json!({"mode":"all_models"}),
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
            ),
            (
                serde_json::json!({"mode":"single_model", "model":"chosen", "effort":"high"}),
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
            ),
            (
                serde_json::json!({"mode":"single_model", "model":"chosen", "effort":"adaptive"}),
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
            ),
            (
                serde_json::json!(true),
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
            ),
            (serde_json::json!(false), SubagentPolicy::Native),
        ] {
            let mut value = launch_json();
            value["subagents"] = policy;
            let launch: WorkerLaunchConfig = serde_json::from_value(value).unwrap();
            assert_eq!(launch.subagents, expected);
        }
    }
}

pub const RELOAD_CANDIDATES_DIR: &str = "reload-candidates";
pub const PENDING_RELOAD_FILE: &str = "pending-reload.json";
pub const RELOAD_CANDIDATE_MANIFEST_FILE: &str = "manifest.json";

pub fn validate_candidate_id(candidate_id: &str) -> Result<()> {
    anyhow::ensure!(
        !candidate_id.is_empty()
            && candidate_id != "."
            && candidate_id != ".."
            && !candidate_id.starts_with(".retired-")
            && !candidate_id.contains(['/', '\\', ':', '\0']),
        "unsafe reload candidate ID"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadSharedLogin {
    pub relative_marker: String,
    pub source_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadCandidateManifest {
    pub operation_id: String,
    pub expected_store_id: String,
    pub candidate_id: String,
    pub harness: HarnessKind,
    pub target_profile_home: String,
    pub clear_paths: Vec<String>,
    pub protected_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_login: Option<ReloadSharedLogin>,
    #[serde(default)]
    pub clear_github_token: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingInPlaceReload {
    pub operation_id: String,
    pub expected_store_id: String,
    pub candidate_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_setup_name: Option<String>,
}

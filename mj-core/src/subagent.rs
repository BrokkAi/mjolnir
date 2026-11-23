//! Durable contracts for Mjolnir-managed child-agent sessions.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::continuation::GeneratedPrompt;
use serde::{Deserialize, Serialize};

/// A session's delegation policy, optionally overriding global task choices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SubagentPolicy {
    #[default]
    Native,
    Mjolnir {
        #[serde(skip_serializing_if = "Option::is_none")]
        straightforward: Option<Box<crate::config::SubagentChoice>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subtle: Option<Box<crate::config::SubagentChoice>>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        orchestrators: bool,
    },
    None,
}

impl<'de> Deserialize<'de> for SubagentPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "mode", rename_all = "snake_case")]
        enum Wire {
            Native,
            Mjolnir {
                #[serde(default)]
                straightforward: Option<Box<crate::config::SubagentChoice>>,
                #[serde(default)]
                subtle: Option<Box<crate::config::SubagentChoice>>,
                #[serde(default)]
                orchestrators: bool,
                // b4374cb29 briefly stored one flattened choice for both types.
                #[serde(default)]
                model: Option<String>,
                #[serde(default)]
                effort: Option<String>,
            },
            #[serde(rename = "all_models", alias = "single_model")]
            LegacyMjolnir,
            None,
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Native => Self::Native,
            Wire::Mjolnir {
                straightforward,
                subtle,
                orchestrators,
                model,
                effort,
            } => {
                if effort.as_deref() == Some("adaptive")
                    || straightforward
                        .as_ref()
                        .and_then(|choice| choice.effort.as_deref())
                        == Some("adaptive")
                    || subtle.as_ref().and_then(|choice| choice.effort.as_deref())
                        == Some("adaptive")
                {
                    return Err(serde::de::Error::custom(
                        "adaptive sub-agent effort is not supported",
                    ));
                }
                if model.is_none() && effort.is_some() {
                    return Err(serde::de::Error::custom(
                        "sub-agent effort requires a model override",
                    ));
                }
                let legacy =
                    model.map(|model| Box::new(crate::config::SubagentChoice { model, effort }));
                Self::Mjolnir {
                    straightforward: straightforward.or_else(|| legacy.clone()),
                    subtle: subtle.or(legacy),
                    orchestrators,
                }
            }
            Wire::LegacyMjolnir => Self::Mjolnir {
                straightforward: None,
                subtle: None,
                orchestrators: false,
            },
            Wire::None => Self::None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTaskType {
    Straightforward,
    Subtle,
}

impl From<WorkerTaskType> for SubagentTaskType {
    fn from(task: WorkerTaskType) -> Self {
        match task {
            WorkerTaskType::Straightforward => Self::Straightforward,
            WorkerTaskType::Subtle => Self::Subtle,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentTaskType {
    #[default]
    Straightforward,
    Subtle,
    Oracle,
    #[serde(rename = "supervisor", alias = "orchestrator")]
    Orchestrator,
}

impl SubagentTaskType {
    pub fn id(self) -> &'static str {
        match self {
            Self::Straightforward => "straightforward",
            Self::Subtle => "subtle",
            Self::Oracle => "oracle",
            Self::Orchestrator => "orchestrator",
        }
    }
}

impl SubagentPolicy {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }
    pub fn uses_mjolnir(&self) -> bool {
        matches!(self, Self::Mjolnir { .. })
    }
    pub fn orchestrators_enabled(&self) -> bool {
        matches!(
            self,
            Self::Mjolnir {
                orchestrators: true,
                ..
            }
        )
    }
    pub fn override_choice(
        &self,
        task_type: SubagentTaskType,
    ) -> Option<&crate::config::SubagentChoice> {
        match self {
            Self::Mjolnir {
                straightforward,
                subtle,
                ..
            } => match task_type {
                SubagentTaskType::Straightforward => straightforward.as_deref(),
                SubagentTaskType::Subtle => subtle.as_deref(),
                SubagentTaskType::Oracle | SubagentTaskType::Orchestrator => None,
            },
            _ => None,
        }
    }
    pub fn suppresses_native(&self) -> bool {
        !self.is_native()
    }
    pub fn parent_role(&self) -> Option<SubagentMcpRole> {
        self.uses_mjolnir().then_some(SubagentMcpRole::Parent)
    }
    /// The policy a new session gets when its creator names none: its
    /// harness's own delegation, or None for a harness that has none.
    pub fn default_for(harness: crate::config::HarnessKind) -> Self {
        Self::Native.as_choice_for(harness)
    }
    /// This policy as the delegation choice shows it for `harness`. A harness
    /// without sub-agents of its own reads Native as None, because that is
    /// what Native does there, so a session stored with Native keeps working.
    pub fn as_choice_for(&self, harness: crate::config::HarnessKind) -> Self {
        if self.is_native() && !harness.has_native_subagents() {
            Self::None
        } else {
            self.clone()
        }
    }
    /// Whether this harness can remember the selected policy for its profile.
    pub fn updates_remembered_choice(&self, harness: crate::config::HarnessKind) -> bool {
        harness.supports_delegation_tools()
    }
    pub fn for_launch(
        &self,
        harness: crate::config::HarnessKind,
        subagent: Option<&SubagentRecord>,
    ) -> Self {
        if !harness.supports_delegation_tools() {
            Self::Native
        } else if let Some(record) = subagent {
            if record.orchestrator {
                let mut policy = self.clone();
                if let Self::Mjolnir { orchestrators, .. } = &mut policy {
                    *orchestrators = false;
                }
                policy
            } else {
                Self::None
            }
        } else {
            self.clone()
        }
    }
}

/// Upgrade compatibility for persisted records and internal handoff only.
/// Public creation APIs deserialize the policy directly and reject booleans.
pub fn deserialize_optional_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SubagentPolicy>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Legacy(bool),
        Policy(SubagentPolicy),
    }
    Ok(
        Option::<Stored>::deserialize(deserializer)?.map(|stored| match stored {
            Stored::Legacy(true) => SubagentPolicy::Mjolnir {
                orchestrators: false,
                straightforward: None,
                subtle: None,
            },
            Stored::Legacy(false) => SubagentPolicy::Native,
            Stored::Policy(policy) => policy,
        }),
    )
}

pub fn deserialize_launch_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SubagentPolicy, D::Error> {
    Ok(deserialize_optional_policy(deserializer)?.unwrap_or_default())
}

/// Read a policy JSON column using the same legacy decoder as durable records.
pub fn parse_stored_policy(json: &str) -> serde_json::Result<SubagentPolicy> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let policy = deserialize_launch_policy(&mut deserializer)?;
    deserializer.end()?;
    Ok(policy)
}

/// Read the remembered per-profile choices while accepting the old scalar
/// field and legacy boolean policy values used by durable snapshots.
pub fn deserialize_policy_map<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<std::collections::BTreeMap<String, SubagentPolicy>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let mut policies = std::collections::BTreeMap::new();
    match value {
        serde_json::Value::Object(values)
            if !values.get("mode").is_some_and(serde_json::Value::is_string) =>
        {
            for (profile, value) in values {
                let policy =
                    parse_stored_policy(&value.to_string()).map_err(serde::de::Error::custom)?;
                policies.insert(profile, policy);
            }
        }
        value => {
            let policy =
                parse_stored_policy(&value.to_string()).map_err(serde::de::Error::custom)?;
            policies.insert(String::new(), policy);
        }
    }
    Ok(policies)
}

/// The refusal code for an unavailable sub-agent choice. A
/// client picks its own remedy from it: the daemon's message names no
/// surface, and only the client knows whether the person has a Settings
/// screen or command-line flags to change the choice with.
pub const CHOICE_UNAVAILABLE_CODE: &str = "subagent_choice_unavailable";

/// How a choice is matched to a profile. Surface-neutral, so it can follow
/// any message from [`SubagentOptions::validate`].
const ELIGIBILITY_NOTE: &str =
    "Mjolnir chooses an enabled profile offering that selection by remaining quota.";

/// Choices before a parent session exists, using the same eligibility as spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentOptions {
    pub models: Vec<crate::acp::SessionConfigChoice>,
    pub efforts: Vec<crate::acp::SessionConfigChoice>,
    pub unavailable: Vec<String>,
}

impl SubagentOptions {
    pub fn validate(&self, choice: &crate::config::SubagentChoice) -> Result<(), String> {
        let crate::config::SubagentChoice { model, effort } = choice;
        if model.trim().is_empty() || crate::model_catalog::resolve(&self.models, model).is_none() {
            return Err(format!(
                "Selected subagent model {model:?} is unavailable.{} {ELIGIBILITY_NOTE}",
                choice_list(" Available models", &self.models)
            ));
        }
        if effort.is_none() {
            return Ok(());
        }
        if effort.as_ref().is_some_and(|effort| {
            effort != "adaptive" && self.efforts.iter().any(|choice| &choice.value == effort)
        }) {
            return Ok(());
        }
        Err(format!(
            "Select an available effort for subagent model {model:?}.{} {ELIGIBILITY_NOTE}",
            choice_list(" Available efforts", &self.efforts)
        ))
    }
}

/// "<label>: a, b." for a non-empty list of choices, otherwise nothing.
fn choice_list(label: &str, choices: &[crate::acp::SessionConfigChoice]) -> String {
    if choices.is_empty() {
        return String::new();
    }
    let values = choices
        .iter()
        .map(|choice| choice.value.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{label}: {values}.")
}

/// How the delegation policy describes spawn choices when both task types are offered.
const TWO_TASK_TYPES: &str = "Mjolnir supplies the configured
child model and effort for each task type. Set `task_type` to `straightforward`
for bounded work with a single clear path: locating and mapping code, running
exact build, test, or gate commands, and small mechanical edits. Use `subtle`
for multi-file implementation (even from a settled design), multi-stage
operational runs or troubleshooting, ambiguous diagnosis, design choices, and
work with a high risk of subtle bugs. A subtle task gets a smarter, more
expensive agent. Judge by scope and branching, not brief length or command
runtime. When a `straightforward` child stalls or returns incomplete after one
correction, escalate that slice to `subtle` rather than taking it over yourself.";

/// The same description when only the straightforward type is configured.
/// There is no type to choose, so the parent delegates straightforward
/// work and keeps work that needs judgment.
const ONE_TASK_TYPE: &str = "Mjolnir supplies the configured
child model and effort. Delegate straightforward work: bounded tasks with a
single clear path, including locating and mapping code, running exact build,
test, or gate commands, and small mechanical edits. Keep multi-file
implementation, troubleshooting, ambiguous diagnosis, design choices, and
changes with a high risk of subtle bugs yourself. Judge by scope and branching,
not brief length or command runtime.";

/// The parent's delegation policy. `offers_subtle` is false when only the
/// straightforward task type is configured.
pub fn delegation_policy(limit: usize, offers_subtle: bool) -> String {
    include_str!("../assets/subagent-delegation.md")
        .replace(
            "$TASK_TYPES",
            if offers_subtle {
                TWO_TASK_TYPES
            } else {
                ONE_TASK_TYPE
            },
        )
        .replace("$N", &limit.to_string())
}

/// The orchestrator's delegation policy document.
pub fn orchestrator_delegation_policy() -> &'static str {
    include_str!("../assets/orchestrator-delegation.md")
}

/// Identity of one finish already sent to the parent in a notification.
///
/// Turn spans distinguish repeated finishes of a child resumed with
/// `send_message`. Terminal failures without a turn span use their state,
/// detail, and the last completed turn ordinal. Session metadata such as a
/// title or updated timestamp does not change the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentFinishIdentity {
    Turn {
        start_position: u64,
        completed_ordinal: u64,
        state: String,
    },
    Terminal {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// The last completed turn separates terminal failures after distinct
        /// runs. Old records stored an `updated_at` string here; consume it as
        /// `None` so those records remain readable without treating renames as
        /// new finishes.
        #[serde(
            default,
            alias = "updated_at",
            deserialize_with = "deserialize_terminal_last_completed_ordinal",
            skip_serializing_if = "Option::is_none"
        )]
        last_completed_ordinal: Option<u64>,
    },
}

impl SubagentFinishIdentity {
    /// Whether a report with this identity was made after the turn that
    /// completed at `ordinal`, or for a later one: the parent has been told of
    /// that turn's ending.
    pub fn covers(&self, ordinal: u64) -> bool {
        self.completed_ordinal()
            .is_some_and(|completed| completed >= ordinal)
    }

    /// The ordinal of the turn this finish completed, whatever the kind of
    /// identity: a terminal one carries the last completed turn, and has none
    /// for a child that never completed a turn. The one place that says which
    /// turn a finish's recorded facts belong to.
    pub fn completed_ordinal(&self) -> Option<u64> {
        match self {
            Self::Turn {
                completed_ordinal, ..
            } => Some(*completed_ordinal),
            Self::Terminal {
                last_completed_ordinal,
                ..
            } => *last_completed_ordinal,
        }
    }
}

fn deserialize_terminal_last_completed_ordinal<'de, D>(
    deserializer: D,
) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| value.as_u64()))
}

/// Guidance for parents whose older workers still call a removed wait tool.
pub const REPORT_NOTIFICATION_NEXT_ACTION: &str = "Each finished child's report arrives as a message from Mjolnir. Do independent work; when none remains, end your turn with a brief waiting status. Your session stays live and children keep running. While you are working reports arrive between tool calls. When you are idle a report starts a new turn. Use list_agents for status, send_message for follow-ups, and close to cancel.";

/// An inclusive, one-based line range within a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineRange {
    pub start: u64,
    pub end: u64,
}

/// One or more line ranges captured from a single file for a child's initial
/// context. Grouping by file lets a parent pull several disjoint ranges out of
/// the same file in one entry, rather than one range per file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSourceRanges {
    pub file: PathBuf,
    pub ranges: Vec<LineRange>,
}

/// One before-push review finding the agent declines to fix, in the agent's
/// own words, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOverride {
    pub finding: String,
    pub reason: String,
}

/// The finding recorded for a queued override from an older worker, which
/// overrode the whole verdict with one reason.
pub const LEGACY_WHOLE_VERDICT_FINDING: &str = "Every finding in the reviewed verdict";

/// One MCP request created inside a parent worker and consumed by the
/// controller. `request_id` is the idempotency identity across reconnects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentToolRequest {
    /// Stamped by the worker owner at durable admission, never trusted from MCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_command_id: Option<String>,
    pub request_id: String,
    pub created_at_ms: i64,
    pub action: SubagentToolAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SubagentToolAction {
    Fetch {
        session_id: String,
        path: PathBuf,
        #[serde(default)]
        ls: bool,
    },
    ListProfiles,
    /// Review the worktrees in `paths` together. Empty means the repository
    /// that contains the session's working directory.
    RequestReview {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    AskOracle {
        message: String,
    },
    /// Decline findings of the outstanding verdict that covers `path`, or of
    /// the only outstanding verdict when no path is given.
    OverrideReview {
        overrides: Vec<ReviewOverride>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Mark commits through `commit` as reviewed, so later reviews start there.
    SetReviewBaseline {
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Spawn {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_github_credentials: bool,
        task_name: String,
        instructions: String,
        #[serde(default)]
        task_type: SubagentTaskType,
        /// Absolute, or relative to the parent session's working directory.
        /// Empty means the parent's own working directory. The directory must
        /// exist on the target; no other restriction applies.
        #[serde(default)]
        working_directory: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileSourceRanges>,
    },
    SpawnOrchestrator {
        task_name: String,
        instructions: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        repos: Option<Vec<String>>,
        #[serde(default)]
        no_github_credentials: bool,
    },
    ListAgents,
    ListTargets,
    SendInput {
        child_session_id: String,
        message: String,
    },
    /// Persisted requests from older workers use this action and its historical
    /// `child_session_id` field for the target. New MCP calls expose
    /// `session_id`, which is mapped here so stored requests remain readable.
    SendMessage {
        child_session_id: String,
        message: String,
    },
    /// Decode-only request from an older worker; reports now arrive as messages.
    #[serde(rename = "wait_agents")]
    LegacyWaitAgents,
    /// Decode-only request from an older worker; no wait is armed.
    #[serde(rename = "await_children")]
    LegacyAwaitChildren,
    /// An accepted request from an older parent worker. New MCP servers do
    /// not expose interrupt, and the daemon rejects this legacy action.
    #[serde(rename = "interrupt_agent")]
    LegacyInterruptAgent {
        child_session_id: String,
    },
    CloseAgent {
        child_session_id: String,
    },
    /// A child's report for the session that started it. Only a child's
    /// worker serves this action; the daemon runs it for the requesting child.
    Handback {
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum SubagentToolActionWire {
    Fetch {
        session_id: String,
        path: PathBuf,
        #[serde(default)]
        ls: bool,
    },
    ListProfiles,
    RequestReview {
        #[serde(default)]
        paths: Vec<String>,
        /// A queued request from an older worker, which named one worktree.
        #[serde(default)]
        path: Option<String>,
    },
    AskOracle {
        message: String,
    },
    OverrideReview {
        #[serde(default)]
        overrides: Vec<ReviewOverride>,
        /// A queued request from an older worker, which overrode the whole
        /// verdict with one reason.
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    SetReviewBaseline {
        commit: String,
        #[serde(default)]
        path: Option<String>,
    },
    Spawn {
        #[serde(default)]
        no_github_credentials: bool,
        task_name: String,
        instructions: String,
        #[serde(default)]
        task_type: SubagentTaskType,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "profile_id")]
        _profile_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "model")]
        _model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "effort")]
        _effort: Option<String>,
        #[serde(default)]
        working_directory: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileSourceRanges>,
    },
    SpawnOrchestrator {
        task_name: String,
        instructions: String,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        repos: Option<Vec<String>>,
        #[serde(default)]
        no_github_credentials: bool,
    },
    ListAgents,
    ListTargets,
    SendInput {
        child_session_id: String,
        message: String,
    },
    SendMessage {
        child_session_id: String,
        message: String,
    },
    #[serde(rename = "wait_agents")]
    LegacyWaitAgents,
    #[serde(rename = "await_children")]
    LegacyAwaitChildren,
    #[serde(rename = "interrupt_agent")]
    LegacyInterruptAgent {
        child_session_id: String,
    },
    CloseAgent {
        child_session_id: String,
    },
    Handback {
        message: String,
    },
}

impl SubagentToolAction {
    /// Whether accepting this action may change a child session's lifecycle or
    /// queued work. In-place Move drains these actions before stopping the
    /// parent worker; observations can be interrupted with the old harness.
    pub const fn mutates_child_state(&self) -> bool {
        matches!(
            self,
            Self::Spawn { .. }
                | Self::SpawnOrchestrator { .. }
                | Self::RequestReview { .. }
                | Self::AskOracle { .. }
                | Self::OverrideReview { .. }
                | Self::SetReviewBaseline { .. }
                | Self::SendInput { .. }
                | Self::SendMessage { .. }
                | Self::CloseAgent { .. }
        )
    }
}

impl<'de> Deserialize<'de> for SubagentToolAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let mut value = serde_json::Value::deserialize(deserializer)?;
        if value.get("action").and_then(serde_json::Value::as_str) == Some("wait_agents") {
            if let Some(params) = value
                .get_mut("params")
                .and_then(serde_json::Value::as_object_mut)
            {
                // Durable worker queues can outlive the build that wrote them.
                // Ignore retired wait fields, but preserve errors for every
                // field that is not part of an old wait request.
                params.remove("child_session_ids");
                params.remove("return_when");
                params.remove("timeout_seconds");
            }
            if value.get("params").is_some_and(|params| {
                params.is_null() || params.as_object().is_some_and(|m| m.is_empty())
            }) {
                value
                    .as_object_mut()
                    .expect("wait action is a JSON object")
                    .remove("params");
            }
        }
        let wire =
            serde_json::from_value::<SubagentToolActionWire>(value).map_err(D::Error::custom)?;
        Ok(match wire {
            SubagentToolActionWire::Fetch {
                session_id,
                path,
                ls,
            } => Self::Fetch {
                session_id,
                path,
                ls,
            },
            SubagentToolActionWire::ListProfiles => Self::ListProfiles,
            SubagentToolActionWire::RequestReview { mut paths, path } => {
                paths.extend(path);
                Self::RequestReview { paths }
            }
            SubagentToolActionWire::AskOracle { message } => Self::AskOracle { message },
            SubagentToolActionWire::OverrideReview {
                mut overrides,
                reason,
                path,
            } => {
                if let Some(reason) = reason {
                    overrides.push(ReviewOverride {
                        finding: LEGACY_WHOLE_VERDICT_FINDING.into(),
                        reason,
                    });
                }
                Self::OverrideReview { overrides, path }
            }
            SubagentToolActionWire::SetReviewBaseline { commit, path } => {
                Self::SetReviewBaseline { commit, path }
            }
            SubagentToolActionWire::Spawn {
                no_github_credentials,
                task_name,
                instructions,
                task_type,
                _profile_id: _,
                _model: _,
                _effort: _,
                working_directory,
                context,
                files,
            } => Self::Spawn {
                no_github_credentials,
                task_name,
                instructions,
                task_type,
                working_directory,
                context,
                files,
            },
            SubagentToolActionWire::SpawnOrchestrator {
                task_name,
                instructions,
                target,
                repos,
                no_github_credentials,
            } => Self::SpawnOrchestrator {
                task_name,
                instructions,
                target,
                repos,
                no_github_credentials,
            },
            SubagentToolActionWire::ListAgents => Self::ListAgents,
            SubagentToolActionWire::ListTargets => Self::ListTargets,
            SubagentToolActionWire::SendInput {
                child_session_id,
                message,
            } => Self::SendInput {
                child_session_id,
                message,
            },
            SubagentToolActionWire::SendMessage {
                child_session_id,
                message,
            } => Self::SendMessage {
                child_session_id,
                message,
            },
            SubagentToolActionWire::LegacyWaitAgents => Self::LegacyWaitAgents,
            SubagentToolActionWire::LegacyAwaitChildren => Self::LegacyAwaitChildren,
            SubagentToolActionWire::LegacyInterruptAgent { child_session_id } => {
                Self::LegacyInterruptAgent { child_session_id }
            }
            SubagentToolActionWire::CloseAgent { child_session_id } => {
                Self::CloseAgent { child_session_id }
            }
            SubagentToolActionWire::Handback { message } => Self::Handback { message },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentToolResult {
    pub request_id: String,
    pub completed_at_ms: i64,
    pub is_error: bool,
    pub message: String,
}

/// Control-plane projection of a completed tool. Report bodies stay on the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentCompletion {
    pub request_id: String,
    pub completed_at_ms: i64,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_delivery: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Legacy worker completion field, retained for decoding.
    pub interrupted_wait: Option<SubagentWaitInterruption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentWaitInterruption {
    pub waited_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
}

impl From<&SubagentToolResult> for SubagentCompletion {
    fn from(result: &SubagentToolResult) -> Self {
        let value = serde_json::from_str::<serde_json::Value>(&result.message).ok();
        let input_delivery = value
            .as_ref()
            .filter(|v| {
                v.get("child_session_id")
                    .is_some_and(serde_json::Value::is_string)
                    && v.get("created_at_ms").is_some()
                    && v.get("status").is_some()
            })
            .map(|v| {
                let mut delivery = serde_json::Map::new();
                for key in [
                    "child_session_id",
                    "created_at_ms",
                    "status",
                    "via",
                    "kind",
                    "turn_id",
                    "command_id",
                    "accepted_ordinal",
                    "error",
                ] {
                    if let Some(value) = v.get(key) {
                        delivery.insert(key.to_owned(), value.clone());
                    }
                }
                serde_json::Value::Object(delivery)
            });
        Self {
            request_id: result.request_id.clone(),
            completed_at_ms: result.completed_at_ms,
            is_error: result.is_error,
            input_delivery,
            interrupted_wait: None,
        }
    }
}

/// A connection exchanges only changed queue entries. Completion replaces a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SubagentQueueChange {
    Request(SubagentToolRequest),
    Completed(SubagentCompletion),
    Retired { request_id: String },
}

impl SubagentQueueChange {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Request(request) => &request.request_id,
            Self::Completed(completion) => &completion.request_id,
            Self::Retired { request_id } => request_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentQueuePage {
    pub sequence: u64,
    pub changes: Vec<SubagentQueueChange>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentKind {
    #[default]
    Worker,
    Reviewer,
    Oracle,
}

impl SubagentKind {
    /// Maximum report length, counted in Unicode characters.
    #[must_use]
    pub fn max_handback_chars(self) -> usize {
        match self {
            Self::Worker => MAX_HANDBACK_CHARS,
            Self::Reviewer | Self::Oracle => MAX_ADVISOR_HANDBACK_CHARS,
        }
    }

    /// Key in the request index; the record keeps the caller's original id.
    #[must_use]
    pub fn request_key(self, key: &str) -> String {
        match self {
            Self::Worker => format!("worker:{key}"),
            Self::Reviewer => format!("reviewer:{key}"),
            Self::Oracle => format!("oracle:{key}"),
        }
    }
}

/// Durable ownership and launch intent for one child session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentRecord {
    #[serde(default)]
    pub kind: SubagentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_launch: Option<crate::worker_launch::ReviewerLaunchConfig>,
    pub child_session_id: String,
    pub parent_session_id: String,
    pub task_name: String,
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Launch directory for the child on the parent's target: absolute, or
    /// relative to the parent session's working directory. Empty means the
    /// parent's own working directory. The directory must exist; no other
    /// restriction applies.
    pub working_directory: PathBuf,
    /// Complete first prompt after the controller captures requested ranges.
    pub initial_prompt: String,
    pub request_key: String,
    pub created_at: String,
    /// The child turn whose completion notice the parent's transcript has
    /// already recorded, so restarts do not repeat the notice. Stored under
    /// the historical field name `delivered_turn`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "delivered_turn"
    )]
    pub noticed_turn: Option<u64>,
    /// The last child finish carried by an admitted parent notification.
    /// Stored in record_json without changing the database schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_finish: Option<SubagentFinishIdentity>,
    /// Whether this child was given the `handback` tool. It is decided once,
    /// when the child is registered. A child recorded before the tool existed
    /// reads as false and keeps reporting through its last message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handback_tool: bool,
    /// Whether this depth-1 worker is an orchestrator that may delegate
    /// to depth-2 workers.
    #[serde(
        rename = "supervisor",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub orchestrator: bool,
    /// The parent suspended this child and should resume it with its own worker.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume_with_parent: bool,
}

/// Relationship and lifecycle metadata published to control surfaces.
/// Complete prompts and reviewer launch configuration remain in the store's
/// `SubagentRecord`; a metadata snapshot cannot create a new child launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentMetadata {
    #[serde(default)]
    pub kind: SubagentKind,
    pub child_session_id: String,
    pub parent_session_id: String,
    pub task_name: String,
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub working_directory: PathBuf,
    pub request_key: String,
    pub created_at: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "delivered_turn"
    )]
    pub noticed_turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_finish: Option<SubagentFinishIdentity>,
    #[serde(default)]
    pub handback_tool: bool,
    #[serde(
        rename = "supervisor",
        alias = "orchestrator",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub orchestrator: bool,
}

impl From<&SubagentRecord> for SubagentMetadata {
    fn from(record: &SubagentRecord) -> Self {
        Self {
            kind: record.kind,
            child_session_id: record.child_session_id.clone(),
            parent_session_id: record.parent_session_id.clone(),
            task_name: record.task_name.clone(),
            profile_id: record.profile_id.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            working_directory: record.working_directory.clone(),
            request_key: record.request_key.clone(),
            created_at: record.created_at.clone(),
            noticed_turn: record.noticed_turn,
            reported_finish: record.reported_finish.clone(),
            handback_tool: record.handback_tool,
            orchestrator: record.orchestrator,
        }
    }
}

impl From<SubagentRecord> for SubagentMetadata {
    fn from(record: SubagentRecord) -> Self {
        Self::from(&record)
    }
}

impl SubagentMetadata {
    /// Apply metadata edits without replacing the durable launch intent.
    pub fn apply_to(&self, record: &mut SubagentRecord) {
        record.kind = self.kind;
        record.child_session_id = self.child_session_id.clone();
        record.parent_session_id = self.parent_session_id.clone();
        record.task_name = self.task_name.clone();
        record.profile_id = self.profile_id.clone();
        record.model = self.model.clone();
        record.effort = self.effort.clone();
        record.working_directory = self.working_directory.clone();
        record.request_key = self.request_key.clone();
        record.created_at = self.created_at.clone();
        record.noticed_turn = self.noticed_turn;
        record.reported_finish = self.reported_finish.clone();
        record.handback_tool = self.handback_tool;
        record.orchestrator = self.orchestrator;
    }

    /// Build a `SubagentRecord` view for shared validation and launch helpers.
    pub fn to_record_view(&self) -> SubagentRecord {
        SubagentRecord {
            resume_with_parent: false,
            kind: self.kind,
            reviewer_launch: None,
            child_session_id: self.child_session_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            task_name: self.task_name.clone(),
            profile_id: self.profile_id.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            working_directory: self.working_directory.clone(),
            initial_prompt: String::new(),
            request_key: self.request_key.clone(),
            created_at: self.created_at.clone(),
            noticed_turn: self.noticed_turn,
            reported_finish: self.reported_finish.clone(),
            handback_tool: self.handback_tool,
            orchestrator: self.orchestrator,
        }
    }
}

/// Shared depth-2 delegation invariant checked by both `State::validate_subagent`
/// and `Controller::register_subagent_inner`.
pub fn validate_subagent_parent(
    parent_subagent: Option<&SubagentRecord>,
    parent_owns_environment: bool,
    requested_kind: SubagentKind,
    requested_orchestrator: bool,
) -> anyhow::Result<()> {
    if requested_orchestrator && requested_kind != SubagentKind::Worker {
        anyhow::bail!("only worker sub-agents can be orchestrators");
    }
    if requested_kind == SubagentKind::Reviewer {
        return Ok(());
    }
    match parent_subagent {
        None => Ok(()),
        Some(parent) if parent.kind == SubagentKind::Worker && parent.orchestrator => {
            if requested_orchestrator {
                anyhow::bail!(
                    "orchestrators cannot spawn nested orchestrators (maximum depth is 2)"
                );
            }
            if requested_kind == SubagentKind::Oracle && !parent_owns_environment {
                anyhow::bail!("borrowed orchestrators cannot spawn oracles");
            }
            Ok(())
        }
        Some(_) => {
            anyhow::bail!("sub-agent sessions cannot spawn their own sub-agents")
        }
    }
}

/// Lifecycle group used by the tool and both user interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Preparing,
    Running,
    InputRequired,
    Completed,
    Failed,
    Interrupted,
    Stopped,
}

impl SubagentRecord {
    #[must_use]
    pub fn is_child(&self, session_id: &str) -> bool {
        self.child_session_id == session_id
    }
}

/// The name a worker's sub-agent MCP server is registered under in every
/// harness, so its tools reach the model as `mcp__mj-agents__<tool>`.
pub const SUBAGENT_MCP_SERVER: &str = "mj-agents";

/// The name of the separate MCP server that offers the before-push review
/// tools, so they reach the model as `mcp__mj-review__<tool>`. Claude loads
/// its tools on demand because they are needed only when a push is near.
pub const REVIEW_MCP_SERVER: &str = "mj-review";

/// The tools the review server lists, in the order it lists them.
pub const REVIEW_MCP_TOOLS: &[&str] = &["request_review", "override_review", "set_baseline"];

/// Which tools a worker's `mj-agents` MCP server offers. A parent may delegate,
/// a top-level session without Mjolnir delegation can message sessions, and a
/// child also hands its report back. Messaging and fetch are common to every
/// role. The review tools belong to the separate `mj-review` server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMcpRole {
    #[default]
    #[serde(alias = "fixed_parent")]
    Parent,
    #[serde(rename = "supervisor", alias = "orchestrator")]
    Orchestrator,
    MessageOnly,
    FetchOnly,
    Child,
}

impl SubagentMcpRole {
    /// The arguments that start the `mj-review` server. Only a managed child
    /// names its role, which selects its instructions; a top-level session's
    /// arguments stay as earlier builds wrote them.
    #[must_use]
    pub fn review_server_args(self, socket: &Path, harness: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "worker",
            "subagent-mcp",
            "--socket",
            &socket.to_string_lossy(),
            "--harness",
            harness,
            "--review-server",
        ]
        .map(str::to_owned)
        .into();
        if matches!(self, Self::Child | Self::FetchOnly | Self::Orchestrator) {
            args.extend(["--role".to_owned(), self.id().to_owned()]);
        }
        args
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Orchestrator => "orchestrator",
            Self::MessageOnly => "message_only",
            Self::FetchOnly => "fetch_only",
            Self::Child => "child",
        }
    }

    /// Tools advertised, accepted and allowed by the harness for this launch.
    #[must_use]
    pub fn tool_names(self, oracle: bool, orchestrators_enabled: bool) -> Vec<&'static str> {
        let mut tools = match self {
            Self::Parent | Self::Orchestrator => {
                let mut tools = vec!["spawn_worker"];
                if self == Self::Parent && orchestrators_enabled {
                    tools.extend(["spawn_orchestrator", "list_targets"]);
                }
                tools.extend(["list_agents", "send_message", "close", "fetch"]);
                tools
            }
            Self::MessageOnly => vec!["send_message", "fetch"],
            Self::FetchOnly => vec!["fetch", "send_message"],
            Self::Child => vec!["handback", "fetch", "send_message"],
        };
        if oracle && self != Self::Child {
            tools.push("ask_oracle");
        }
        if self == Self::Orchestrator {
            tools.push("handback");
        }
        tools
    }
}

impl std::fmt::Display for SubagentMcpRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id())
    }
}

impl std::str::FromStr for SubagentMcpRole {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "parent" => Ok(Self::Parent),
            "fixed_parent" => Ok(Self::Parent),
            "orchestrator" | "supervisor" => Ok(Self::Orchestrator),
            "message_only" => Ok(Self::MessageOnly),
            "fetch_only" => Ok(Self::FetchOnly),
            "child" => Ok(Self::Child),
            other => anyhow::bail!("unknown sub-agent MCP role {other:?}"),
        }
    }
}

/// The reminder itself, sent as an ordinary prompt so the child's transcript
/// shows it. A turn can end before the task is done (a harness failure, a
/// question in plain text), so the reminder must not order a child that has
/// not finished to stop: that turned a failed first turn into an empty report
/// (issue 1217).
pub const HANDBACK_REMINDER_TEXT: &str = "[handback reminder] Your turn ended without delivering a report. If your assigned task is finished, call the mj-agents handback tool now with your full report, then stop. If it is not finished, continue working on it, and hand back when it is done.";

/// A child's delegated authority and reporting requirements. Shared
/// by the first-prompt note, the child's server instructions and the handback
/// tool description, so the three cannot drift apart.
pub static HANDBACK_REPORT_RULES: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Your parent reads your report, at most {} characters for workers or {} for oracles and reviewers: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Put longer logs, tables and findings in your report directory; list their paths instead of copying them into the report. Keep text evidence here, not build outputs, binaries or downloads. Handback is refused above the directory's byte limit. Fix compile, lint and format errors and tests your changes break; list each fix in one report line. Complete the assigned outcome within the parent's agreed design and constraints. Resolve routine implementation details and perform necessary supporting work, including edits and tests in files or artifacts not named explicitly. Named files are starting pointers unless explicitly declared an ownership boundary. Respect read-only assignments, explicit exclusions, and other agents' ownership; do not overwrite concurrent changes. Ask the parent before changing agreed behavior or design, introducing an unapproved public or persisted contract change, crossing an ownership boundary, or taking an action outside delegated authority. Changes already required by the assignment, including contract or epoch changes, need no duplicate approval. Report unrelated baseline failures and continue independent assigned work. When blocked on a parent decision, hand back the question and decisive evidence. The parent owns consequential design decisions and final acceptance. A question you ask is sent to your parent and closed right away; its answer arrives as a message.",
        SubagentKind::Worker.max_handback_chars(),
        SubagentKind::Oracle.max_handback_chars(),
    )
});

/// Assignment safety scope also applies when an adapter cannot receive standing instructions.
pub const READ_ONLY_TASK_SCOPE: &str =
    "This is a read-only consultation: do not edit files, build, or run tests.";

/// Per-task directory note; the standing role contract travels separately.
#[must_use]
pub fn report_directory_note(report_dir: &str) -> String {
    format!("Your report directory is {report_dir}.")
}

/// What a child's first prompt opens with when it has the tool. It leads the
/// prompt rather than trailing a parent's long instructions, where live runs
/// showed children skipping it. `report_dir` is the directory Mjolnir created
/// for this child's files. The sentence about long waits is here and not in
/// [`HANDBACK_REPORT_RULES`] because those rules already fill the length the
/// harnesses keep of a tool description and of server instructions.
#[must_use]
pub fn handback_prompt_note(report_dir: &str) -> String {
    format!(
        "You are a Mjolnir sub-agent. Finish every task by calling the mj-agents handback tool with your report: the session that started you reads only that report, not the rest of this conversation. When your assignment needs the result of a build, test run or other long command, wait for it to finish: a long wait is not a reason to hand back. Your report directory is {report_dir}. {}",
        HANDBACK_REPORT_RULES.as_str()
    )
}

/// How long a sent reminder may be neither queued, running nor finished before
/// the child's report stops waiting for it. Someone removed it from the queue,
/// or it never reached the child; either way it is not coming.
pub const HANDBACK_REMINDER_GRACE_MS: i64 = 30_000;

/// The longest ordinary worker report a handback or notification includes.
/// Details belong in files in the child's report directory.
pub const MAX_HANDBACK_CHARS: usize = 4_000;

/// Oracles and reviewers return complete advice and findings in one handback.
pub const MAX_ADVISOR_HANDBACK_CHARS: usize = 64 * 1024;

/// Directory under a parent's workspace root that holds its children's report
/// directories, outside every repository.
pub const REPORT_ROOT_DIR: &str = ".mj-agents";

/// For a session whose workspace root is not Mjolnir's (a bare project), the
/// report root sits inside the project, under a path its `info/exclude` lists.
pub const PROJECT_REPORT_ROOT_DIR: &str = ".mj/agents";

/// Archive prefix for the report files a checkpoint carries.
pub const ARCHIVE_REPORT_DIR: &str = "mj-agent-reports";

/// The most sub-agent report bytes one checkpoint carries. Reports are the
/// details a child's short handback points to, so a checkpoint keeps the
/// newest files up to this bound rather than failing over them.
pub const MAX_CHECKPOINT_REPORT_BYTES: u64 = 64 * 1024 * 1024;

/// Controller-authored launch environment used by every child's MCP server.
pub const CHILD_REPORT_DIR_ENV: &str = "MJ_CHILD_REPORT_DIR";
pub const CHILD_REPORT_LIMIT_ENV: &str = "MJ_CHILD_REPORT_LIMIT_BYTES";

/// The most bytes one child's report directory may hold when it hands back:
/// an equal share of [`MAX_CHECKPOINT_REPORT_BYTES`] for each child that can
/// run at once, so the live children's reports fit in one checkpoint.
#[must_use]
pub fn max_child_report_bytes(max_concurrent_children: usize) -> u64 {
    MAX_CHECKPOINT_REPORT_BYTES / max_concurrent_children.max(1) as u64
}

/// Cut `text` to [`MAX_HANDBACK_CHARS`] characters, saying how much was left
/// out and how the parent can get it. Returns the text and whether it was cut.
#[must_use]
pub fn bounded_report(text: &str) -> (String, bool) {
    bounded_report_with_recovery(
        text,
        SubagentKind::Worker.max_handback_chars(),
        "use send_message to ask the child to write the details to files in its report directory and send you the paths",
    )
}

/// Bound an oracle answer while keeping its recovery channel in the oracle.
#[must_use]
pub fn bounded_oracle_report(text: &str) -> (String, bool) {
    bounded_report_with_recovery(
        text,
        SubagentKind::Oracle.max_handback_chars(),
        "use ask_oracle to ask for the rest; the oracle keeps its context",
    )
}

fn bounded_report_with_recovery(text: &str, maximum: usize, recovery: &str) -> (String, bool) {
    let total = text.chars().count();
    if total <= maximum {
        return (text.to_owned(), false);
    }
    let kept: String = text.chars().take(maximum).collect();
    (
        format!(
            "{kept}\n[truncated: {} more characters; {recovery}]",
            total - maximum
        ),
        true,
    )
}

/// How long Mjolnir holds a child that ended its turn to wait for processes it
/// started before asking it to hand back what it has. The user chose 30
/// minutes on 2026-11-16: the measured 90th percentile of background work that
/// agents waited for was 30 minutes (1,057 tasks since 2026-11-01; 99th
/// percentile 115 minutes), and a longer run survives because a park waits for
/// the parent's next turn, in which the parent can tell the child to continue.
pub const HOLD_GIVE_UP_MS: i64 = 30 * 60 * 1000;

/// Which follow-up prompt Mjolnir sends a child it is holding while the child
/// waits for its own background processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldFollowUpKind {
    /// Every process the child was waiting for has exited.
    ProcessesExited,
    /// The child waited [`HOLD_GIVE_UP_MS`] and should hand back what it has.
    GiveUp,
}

/// Whether a command id names the prompt that reminds a child to hand back its
/// report, which is how [`report_state`] tells it from the task it follows up.
#[must_use]
pub fn is_handback_reminder(command_id: &str) -> bool {
    let command_id = crate::continuation::carried_command(command_id);
    GeneratedPrompt::HandbackReminder
        .suffix(command_id)
        .is_some()
}

/// The command id of the handback reminder for the turn that finished at
/// `completed_ordinal`. The id is deterministic so a reminder is sent at most
/// once per turn, and [`reminded_turn_ordinal`] reads it back.
#[must_use]
pub fn handback_reminder_command_id(completed_ordinal: u64) -> String {
    GeneratedPrompt::HandbackReminder.command_id(completed_ordinal)
}

/// The ordinal at which the reminded turn finished, when `command_id` names a
/// handback reminder.
#[must_use]
pub fn reminded_turn_ordinal(command_id: &str) -> Option<u64> {
    let command_id = crate::continuation::carried_command(command_id);
    GeneratedPrompt::HandbackReminder
        .suffix(command_id)?
        .parse()
        .ok()
}

impl HoldFollowUpKind {
    fn prompt(self) -> GeneratedPrompt {
        match self {
            Self::ProcessesExited => GeneratedPrompt::HoldWake,
            Self::GiveUp => GeneratedPrompt::HoldGiveUp,
        }
    }
}

/// The deterministic command id of the follow-up of `kind` for the reminder
/// turn that finished at `reminder_completed_ordinal`. The id makes a retry
/// idempotent and lets a restarted daemon see what it already sent.
#[must_use]
pub fn hold_follow_up_command_id(
    kind: HoldFollowUpKind,
    reminder_completed_ordinal: u64,
) -> String {
    kind.prompt().command_id(reminder_completed_ordinal)
}

/// The kind and reminder-turn ordinal a hold follow-up command id names.
#[must_use]
pub fn hold_follow_up_of(command_id: &str) -> Option<(HoldFollowUpKind, u64)> {
    let command_id = crate::continuation::carried_command(command_id);
    [HoldFollowUpKind::ProcessesExited, HoldFollowUpKind::GiveUp]
        .into_iter()
        .find_map(|kind| {
            let ordinal = kind.prompt().suffix(command_id)?.parse().ok()?;
            Some((kind, ordinal))
        })
}

/// A report a child handed back during the turn `command_id` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentHandback {
    pub command_id: String,
    pub message: String,
    pub recorded_at_ms: i64,
}

impl SubagentHandback {
    /// Whether this report answers the turn `command_id` names. A report from
    /// an earlier or later task of the same child answers none of the others.
    #[must_use]
    pub fn answers(&self, command_id: &str) -> bool {
        report_answers(&self.command_id, command_id)
    }
}

/// Whether a report recorded for `report_command_id` answers the turn
/// `command_id` names; a restart continuation is the turn it carries out.
#[must_use]
pub fn report_answers(report_command_id: &str, command_id: &str) -> bool {
    crate::continuation::carried_command(report_command_id)
        == crate::continuation::carried_command(command_id)
}

/// The reminder Mjolnir sent after the turn `for_command_id` ended without a
/// report. `command_id` is the reminder prompt's own command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandbackReminder {
    pub command_id: String,
    pub for_command_id: String,
    pub sent_at_ms: i64,
}

/// Everything recorded about one child's report. Each part keeps only its
/// latest value: a report answers the newest task, and each task gets at most
/// one reminder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub handback: Option<SubagentHandback>,
    pub reminder: Option<HandbackReminder>,
    /// The turn whose reminder could not be sent.
    pub reminder_failed_for: Option<String>,
    /// Acceptance ordinal of the newest prompt the parent gave this child.
    /// The child has not answered it until a finished turn reaches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaited_ordinal: Option<u64>,
    /// The absolute directory on the parent's target where this child writes
    /// the details its report points to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_dir: Option<String>,
}

/// Whether the parent's newest prompt has yet to be answered: no finished
/// turn has reached its acceptance ordinal. The store learns of a new turn a
/// moment after the prompt is accepted, and in that gap an idle child would
/// otherwise read as finished with the previous turn's report, or none.
#[must_use]
pub fn awaiting_prompt(
    awaited_ordinal: Option<u64>,
    last_turn: Option<&crate::state::MaterializedTurnOutcome>,
) -> bool {
    awaited_ordinal.is_some_and(|ordinal| last_turn.is_none_or(|turn| !turn.answers(ordinal)))
}

/// How a child's last finished turn failed, if it did: `interrupted` for a
/// cancelled or interrupted turn, `failed` for an error, a refusal or a quota
/// stop, with the most specific reason recorded. A turn that finished or
/// stopped to ask for input did not fail.
#[must_use]
pub fn failed_turn(
    turn: &crate::state::MaterializedTurnOutcome,
    last_message: Option<&str>,
) -> Option<(&'static str, String)> {
    use crate::state::{PromptCompletion, TurnOutcomeKind, classify_prompt_completion};
    let (state, fallback) = match &turn.outcome {
        TurnOutcomeKind::Completed { stop_reason } => match classify_prompt_completion(stop_reason)
        {
            PromptCompletion::Finished | PromptCompletion::InputRequired => return None,
            PromptCompletion::Cancelled => ("interrupted", "the turn was cancelled".to_owned()),
            PromptCompletion::QuotaLimit | PromptCompletion::Error => (
                "failed",
                format!("the turn ended with stop reason {stop_reason:?}"),
            ),
        },
        TurnOutcomeKind::Interrupted { message, .. } => ("interrupted", message.clone()),
        TurnOutcomeKind::Rejected { message, .. } => ("failed", message.clone()),
    };
    let reason = turn
        .diagnostic
        .as_ref()
        .map(|diagnostic| diagnostic.message.clone())
        .filter(|message| !message.trim().is_empty())
        .or_else(|| {
            last_message
                .filter(|message| !message.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or(fallback);
    Some((state, reason))
}

/// Whether a child's last finished turn reported an authentication failure.
/// The diagnostic indicates it (an error kind such as
/// Codex's `unauthorized`, or an auth failure sentence), or the turn failed
/// with such a sentence as its reason. A usage limit is never one.
///
/// Only the recorded failure is read, never the child's own messages: a child
/// may well be writing about authentication errors.
#[must_use]
pub fn turn_failed_on_login(turn: &crate::state::MaterializedTurnOutcome) -> bool {
    if turn
        .diagnostic
        .as_ref()
        .is_some_and(crate::credentials::turn_diagnostic_reports_auth_failure)
    {
        return true;
    }
    failed_turn(turn, None).is_some_and(|(state, reason)| {
        state == "failed" && crate::credentials::text_reports_auth_failure(&reason)
    })
}

/// Authentication evidence from a wake that staged an identifiable login.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeFailure {
    Authentication {
        profile_id: String,
        staged_login: crate::credentials::LoginFingerprint,
    },
}

impl WakeFailure {
    const SEPARATOR: &str = "\nMjolnir wake evidence: ";

    /// Keep the existing human-readable detail first for older readers.
    pub fn encode(&self, detail: &str) -> serde_json::Result<String> {
        Ok(format!(
            "{detail}{}{}",
            Self::SEPARATOR,
            serde_json::to_string(self)?
        ))
    }

    pub fn decode(error: &str) -> (&str, Option<Self>) {
        error
            .rsplit_once(Self::SEPARATOR)
            .and_then(|(detail, evidence)| Some((detail, serde_json::from_str(evidence).ok()?)))
            .map_or((error, None), |(detail, evidence)| (detail, Some(evidence)))
    }
}

/// Guidance for a parent whose child's lifecycle is in Error.
pub const FAILED_CHILD_CLOSE_GUIDANCE: &str =
    "Use close to close this child, then spawn a replacement if needed.";

/// What a parent is told about a profile whose login the provider refused.
#[must_use]
pub fn login_invalid_reason(profile_id: &str) -> String {
    format!("the login is no longer valid; run `mj login --profile {profile_id}` and spawn again")
}

/// What a caller knows of the processes a child's last turn left running.
/// "No worker to ask" is not "nothing is running": a reader that cannot reach
/// the child's worker must not report a child that may be waiting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Liveness {
    /// The child's worker is not attached to this reader.
    #[default]
    Unknown,
    /// The connected worker's publication: `None` when it publishes no
    /// liveness (no processes were left, or the worker predates protocol 67).
    Known(Option<crate::work_facts::WorkLiveness>),
}

/// Where a child's report stands after its last finished turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportState {
    /// The child handed back this report for its last turn.
    Delivered(String),
    /// The child owes a report. `remind` says no reminder has been sent for
    /// this turn yet; otherwise one is on its way.
    Pending { remind: bool },
    /// The child ended its reminder turn without a report while processes it
    /// started are still running (or just exited): it is waiting, not done.
    /// No report is owed to the parent and the child is not parked.
    Held {
        /// When the reminder turn ended, which is when the hold began.
        since_ms: i64,
        /// When the hold gives up and asks the child to hand back.
        give_up_at_ms: i64,
        /// The processes the child waits on, rendered one per line.
        waiting_on: Vec<String>,
        /// The follow-up prompt due now, if any. `None` while still waiting.
        follow_up: Option<HoldFollowUpKind>,
    },
    /// No handback is coming: report the turn's last message, as before.
    Fallback,
}

/// The processes a finished turn's work facts list as running, one per line.
#[must_use]
pub fn running_process_lines(facts: &crate::work_facts::WorkFacts) -> Vec<String> {
    let Some(processes) = facts.processes.known() else {
        return Vec::new();
    };
    let mut lines: Vec<String> = processes
        .shown
        .iter()
        .map(crate::work_facts::render_process)
        .collect();
    if processes.omitted > 0 {
        lines.push(format!("and {} more", processes.omitted));
    }
    lines
}

/// The rule for a child's report, shared by notifications, session waiting
/// and the reminder.
///
/// `in_flight` names the commands queued or running on the child, so a sent
/// reminder is known to be on its way. A child without the tool, and a turn
/// that did not finish normally, fall back to the last message; a child that
/// already had its one reminder does too, unless the reminder turn ended with
/// processes the child started still alive (`liveness` names that turn): then
/// the child is held, see [`ReportState::Held`]. A worker that publishes no
/// liveness never holds a child.
#[must_use]
pub fn report_state(
    handback_tool: bool,
    report: &SubagentReport,
    last_turn: Option<&crate::state::MaterializedTurnOutcome>,
    liveness: &Liveness,
    in_flight: &[&str],
    now_ms: i64,
) -> ReportState {
    let Some(turn) = last_turn.filter(|_| handback_tool) else {
        return ReportState::Fallback;
    };
    if let Some(handback) = report
        .handback
        .as_ref()
        .filter(|handback| handback.answers(&turn.command_id))
    {
        return ReportState::Delivered(handback.message.clone());
    }
    let finished = matches!(
        &turn.outcome,
        crate::state::TurnOutcomeKind::Completed { stop_reason }
            if crate::state::classify_prompt_completion(stop_reason)
                == crate::state::PromptCompletion::Finished
    );
    // A follow-up the worker refused as stale ends the hold: the child's last
    // message is reported.
    if finished
        && is_handback_reminder(&turn.command_id)
        && report.reminder_failed_for.as_deref() != Some(turn.command_id.as_str())
    {
        let follow_up_in_flight = [HoldFollowUpKind::ProcessesExited, HoldFollowUpKind::GiveUp]
            .into_iter()
            .any(|kind| {
                in_flight
                    .contains(&hold_follow_up_command_id(kind, turn.completed_ordinal).as_str())
            });
        if follow_up_in_flight {
            return ReportState::Pending { remind: false };
        }
        let liveness = match liveness {
            // Nobody can say whether the child still waits: not an ending.
            Liveness::Unknown => return ReportState::Pending { remind: false },
            Liveness::Known(known) => known
                .as_ref()
                .filter(|l| l.completed_ordinal == turn.completed_ordinal),
        };
        let Some(liveness) = liveness else {
            return ReportState::Fallback;
        };
        let give_up_at_ms = turn.completed_at_ms.saturating_add(HOLD_GIVE_UP_MS);
        let follow_up = if liveness.alive == 0 {
            Some(HoldFollowUpKind::ProcessesExited)
        } else if now_ms >= give_up_at_ms {
            Some(HoldFollowUpKind::GiveUp)
        } else {
            None
        };
        return ReportState::Held {
            since_ms: turn.completed_at_ms,
            give_up_at_ms,
            waiting_on: turn
                .work_facts
                .as_ref()
                .map(running_process_lines)
                .unwrap_or_default(),
            follow_up,
        };
    }
    if !finished
        || is_handback_reminder(&turn.command_id)
        || matches!(
            hold_follow_up_of(&turn.command_id),
            Some((HoldFollowUpKind::GiveUp, _))
        )
        || report.reminder_failed_for.as_deref() == Some(turn.command_id.as_str())
    {
        return ReportState::Fallback;
    }
    match report
        .reminder
        .as_ref()
        .filter(|reminder| reminder.for_command_id == turn.command_id)
    {
        None => ReportState::Pending { remind: true },
        Some(reminder)
            if in_flight.contains(&reminder.command_id.as_str())
                || now_ms.saturating_sub(reminder.sent_at_ms) < HANDBACK_REMINDER_GRACE_MS =>
        {
            ReportState::Pending { remind: false }
        }
        Some(_) => ReportState::Fallback,
    }
}

/// A sub-agent that Mjolnir stopped when its parent was suspended. The
/// parent's record keeps one of these per child until the parent's model has
/// been told, on the first prompt after the parent resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedSubagent {
    pub child_session_id: String,
    /// The child's listed title when it was stopped.
    pub title: String,
    /// One line of the task the parent gave it, when the spawn recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Whether the child had handed back its report for the parent's newest
    /// task; see [`has_handed_back`].
    pub handed_back: bool,
}

/// The key under which a stopped oracle is kept in the parent's stopped list.
/// It stands in for the oracle's session ID, so that ID never reaches the
/// primary: the list is rendered into its prompt context.
pub const STOPPED_ORACLE_ID: &str = "oracle";

/// The one line a primary is told when its oracle was stopped.
pub const STOPPED_ORACLE_LINE: &str = "The oracle was stopped. If you were waiting for its answer, ask again; a fresh oracle will answer.";

impl StoppedSubagent {
    /// The list entry for a stopped oracle: no session ID and no prompt.
    #[must_use]
    pub fn oracle() -> Self {
        Self {
            child_session_id: STOPPED_ORACLE_ID.into(),
            title: "oracle".into(),
            task: None,
            handed_back: true,
        }
    }

    /// Whether this entry is the stopped oracle, which is rendered as
    /// [`STOPPED_ORACLE_LINE`] and never as a child.
    #[must_use]
    pub fn is_oracle(&self) -> bool {
        self.child_session_id == STOPPED_ORACLE_ID
    }
}

/// A child whose parent remains live across an in-place harness swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlaceSubagent {
    pub child_session_id: String,
    /// The child's name as the session list shows it.
    pub name: String,
    pub state: InPlaceSubagentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlaceSubagentState {
    Running,
    Parked,
}

impl InPlaceSubagentState {
    const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Parked => "parked",
        }
    }
}

/// Reserved hidden-context tag for children retained across an in-place swap.
pub const IN_PLACE_SUBAGENTS_TAG: &str = "mj-in-place-subagents";

fn safe_subagent_name(name: &str) -> String {
    crate::state::bounded_one_line(name, STOPPED_TASK_CHARS)
        .replace('<', "‹")
        .replace('>', "›")
}

/// Hidden instructions for the first prompt after an in-place swap. The new
/// harness has no outstanding tool call stack from the old harness process.
#[must_use]
pub fn in_place_subagents_prompt_context(children: &[InPlaceSubagent]) -> Option<String> {
    if children.is_empty() {
        return None;
    }
    let mut lines = vec![
        format!("<{IN_PLACE_SUBAGENTS_TAG}>"),
        "These sub-agents remain attached to this session after its in-place harness swap:".into(),
    ];
    for child in children {
        lines.push(format!(
            "- {} (child_session_id {}; {})",
            safe_subagent_name(&child.name),
            child.child_session_id,
            child.state.label()
        ));
    }
    lines.push(
        "Any wait that was in progress during the swap was interrupted; reissue wait without arguments to collect reports from these children.".into(),
    );
    lines.push(
        "Before resending a message, call list_agents and check pending_messages so you do not send it twice.".into(),
    );
    lines.push(format!("</{IN_PLACE_SUBAGENTS_TAG}>"));
    Some(lines.join("\n"))
}

/// Conversation line that tells the person the same children are still live.
#[must_use]
pub fn in_place_subagents_notice(children: &[InPlaceSubagent]) -> Option<String> {
    if children.is_empty() {
        return None;
    }
    let count = children.len();
    let noun = if count == 1 {
        "sub-agent"
    } else {
        "sub-agents"
    };
    Some(format!(
        "In-place harness swap kept {count} {noun} attached. Any wait in progress was interrupted; reissue wait without arguments to collect reports. Check list_agents for pending_messages before resending a message."
    ))
}

/// Longest task line a [`StoppedSubagent`] keeps, in characters.
pub const STOPPED_TASK_CHARS: usize = 160;

/// How [`handback_prompt_note`] begins, whatever directory it names.
const HANDBACK_NOTE_OPENING: &str = "You are a Mjolnir sub-agent. ";

/// One line of the task a parent gave a child, from the child's first prompt:
/// the parent's instructions without the handback note Mjolnir puts before
/// them, and without the context and file ranges the parent attached after
/// them. `report_dir` is the directory that note names, when one was recorded.
#[must_use]
pub fn task_summary(initial_prompt: &str, report_dir: Option<&str>) -> Option<String> {
    let mut task = initial_prompt;
    if let Some(rest) = report_dir.and_then(|directory| {
        task.strip_prefix(report_directory_note(directory).as_str())
            .and_then(|rest| rest.strip_prefix("\n\n"))
    }) {
        task = rest;
    }
    if let Some(rest) = report_dir.and_then(|directory| {
        task.strip_prefix(handback_prompt_note(directory).as_str())
            .and_then(|rest| rest.strip_prefix("\n\n"))
    }) {
        task = rest;
    } else if task.starts_with(HANDBACK_NOTE_OPENING)
        && let Some((_, rest)) = task.split_once("\n\n")
    {
        task = rest;
    }
    if let Some(rest) = task
        .strip_prefix(READ_ONLY_TASK_SCOPE)
        .and_then(|rest| rest.strip_prefix("\n\n"))
    {
        task = rest;
    }
    for attachment in ["\n\n<parent_context>", "\n\n--- source "] {
        if let Some((instructions, _)) = task.split_once(attachment) {
            task = instructions;
        }
    }
    let line = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= STOPPED_TASK_CHARS {
        return Some(line);
    }
    let kept: String = line.chars().take(STOPPED_TASK_CHARS - 1).collect();
    Some(format!("{}…", kept.trim_end()))
}

/// The tag of the hidden block that tells a resumed parent's model which
/// sub-agents its suspend stopped. It is a reserved `mj-` block, so
/// [`crate::relay::strip_hidden_prompt_context`] removes it wherever a harness
/// copied the prompt into text a person sees.
pub const STOPPED_SUBAGENTS_TAG: &str = "mj-stopped-subagents";

/// The hidden context the first prompt after a parent resumes carries when
/// its suspend stopped sub-agents: one line per child, then what the model
/// should do about them. `None` when nothing was stopped.
#[must_use]
pub fn stopped_subagents_prompt_context(stopped: &[StoppedSubagent]) -> Option<String> {
    if stopped.is_empty() {
        return None;
    }
    let oracle_stopped = stopped.iter().any(StoppedSubagent::is_oracle);
    let stopped = stopped
        .iter()
        .filter(|child| !child.is_oracle())
        .cloned()
        .collect::<Vec<_>>();
    if stopped.is_empty() {
        return Some(format!(
            "<{STOPPED_SUBAGENTS_TAG}>\n{STOPPED_ORACLE_LINE}\n</{STOPPED_SUBAGENTS_TAG}>"
        ));
    }
    let total = stopped.len();
    let not_handed_back = stopped.iter().filter(|child| !child.handed_back).count();
    let mut lines = vec![
        format!("<{STOPPED_SUBAGENTS_TAG}>"),
        format!(
            "Mjolnir stopped {} when this session was suspended:",
            crate::text::counted(total, "sub-agent", "sub-agents")
        ),
    ];
    for child in &stopped {
        let task = child
            .task
            .as_deref()
            .map(|task| format!(", task: {task}"))
            .unwrap_or_default();
        let handed_back = if child.handed_back {
            "had handed back"
        } else {
            "had not handed back"
        };
        lines.push(format!(
            "- \"{}\" (child_session_id {}){task}; {handed_back}",
            child.title, child.child_session_id
        ));
    }
    lines.push(
        match (total, not_handed_back) {
            (1, 1) => "Its work was not handed back; spawn it again if you still need it.",
            (1, _) => "It had handed back its report before it was stopped.",
            (_, 0) => "Each had handed back its report before it was stopped.",
            (total, count) if total == count => {
                "Their work was not handed back; spawn them again if you still need it."
            }
            _ => {
                "The work of those that had not handed back was lost; spawn them again if you still need it."
            }
        }
        .to_owned(),
    );
    lines.push(
        "A stopped sub-agent no longer exists: wait, send_message and close cannot reach it."
            .to_owned(),
    );
    if oracle_stopped {
        lines.push(STOPPED_ORACLE_LINE.to_owned());
    }
    lines.push(format!("</{STOPPED_SUBAGENTS_TAG}>"));
    Some(lines.join("\n"))
}

/// The conversation line a person sees when a parent whose suspend stopped
/// sub-agents resumes. It names only the children that had not handed back,
/// since those lost work; the idle ones that had are counted. `None` when
/// nothing was stopped.
#[must_use]
pub fn stopped_subagents_notice(stopped: &[StoppedSubagent]) -> Option<String> {
    if stopped.is_empty() {
        return None;
    }
    let oracle_stopped = stopped.iter().any(StoppedSubagent::is_oracle);
    let children = stopped
        .iter()
        .filter(|child| !child.is_oracle())
        .cloned()
        .collect::<Vec<_>>();
    if !oracle_stopped {
        return stopped_children_notice(&children);
    }
    Some(match stopped_children_notice(&children) {
        Some(notice) => format!("{notice} {STOPPED_ORACLE_LINE}"),
        None => STOPPED_ORACLE_LINE.to_owned(),
    })
}

fn stopped_children_notice(stopped: &[StoppedSubagent]) -> Option<String> {
    if stopped.is_empty() {
        return None;
    }
    let working = stopped
        .iter()
        .filter(|child| !child.handed_back)
        .map(|child| format!("\"{}\"", child.title))
        .collect::<Vec<_>>();
    let idle = stopped.len() - working.len();
    if working.is_empty() {
        return Some(format!(
            "Suspend stopped {}.",
            crate::text::counted(idle, "idle sub-agent", "idle sub-agents")
        ));
    }
    let mut notice = format!(
        "Suspend stopped {}: {}",
        crate::text::counted(working.len(), "working sub-agent", "working sub-agents"),
        working.join(", ")
    );
    if idle > 0 {
        notice.push_str(&format!(
            "; {} stopped too",
            crate::text::counted(idle, "idle sub-agent was", "idle sub-agents were")
        ));
    }
    notice.push('.');
    Some(notice)
}

/// What a person is told before a suspend stops sub-agents that have not
/// handed back their reports. `None` when every child had handed back: those
/// are stopped without a word, since their reports already reached the parent.
#[must_use]
pub fn suspend_warning(not_handed_back: usize) -> Option<String> {
    match not_handed_back {
        0 => None,
        1 => Some("1 sub-agent has not handed back; suspending stops it".to_owned()),
        count => Some(format!(
            "{count} sub-agents have not handed back; suspending stops them"
        )),
    }
}

/// The same warning naming the children by their listed titles, for a
/// person choosing whether to suspend: up to three by name, then how many
/// more. The API keeps [`suspend_warning`]'s count.
#[must_use]
pub fn suspend_warning_naming(titles: &[String]) -> Option<String> {
    match titles.len() {
        0 => None,
        1 => Some(format!(
            "Sub-agent {} has not handed back; suspending stops it",
            quoted_titles(titles)
        )),
        _ => Some(format!(
            "Sub-agents {} have not handed back; suspending stops them",
            quoted_titles(titles)
        )),
    }
}

/// Titles in quotes, in a sentence: `"a"`, `"a" and "b"`, `"a", "b" and
/// "c"`. Past three, the rest are counted: `"a", "b", "c" and 2 more`.
#[must_use]
pub fn quoted_titles(titles: &[String]) -> String {
    const NAMED: usize = 3;
    let mut named = titles
        .iter()
        .take(NAMED)
        .map(|title| format!("\"{title}\""))
        .collect::<Vec<_>>();
    let last = if titles.len() > NAMED {
        format!("{} more", titles.len() - NAMED)
    } else {
        match named.pop() {
            Some(last) => last,
            None => return String::new(),
        }
    };
    if named.is_empty() {
        last
    } else {
        format!("{} and {last}", named.join(", "))
    }
}

/// Whether a child had handed back its report for the parent's newest task:
/// it is not working, a finished turn answers the parent's newest prompt, and
/// that answer is final. The answer is the report the child handed back, or,
/// for a child without the tool or one that ignored its reminder, the last
/// message of a turn that finished normally. A turn that failed, was
/// interrupted or stopped to ask a question has not handed anything back.
///
/// `working` says the child has a turn running or its execution is not idle.
#[must_use]
pub fn has_handed_back(
    handback_tool: bool,
    report: &SubagentReport,
    working: bool,
    last_turn: Option<&crate::state::MaterializedTurnOutcome>,
    liveness: &Liveness,
    now_ms: i64,
) -> bool {
    let Some(turn) = last_turn else {
        return false;
    };
    if working || awaiting_prompt(report.awaited_ordinal, Some(turn)) {
        return false;
    }
    match report_state(handback_tool, report, Some(turn), liveness, &[], now_ms) {
        ReportState::Delivered(_) => true,
        ReportState::Pending { .. } | ReportState::Held { .. } => false,
        ReportState::Fallback => matches!(
            &turn.outcome,
            crate::state::TurnOutcomeKind::Completed { stop_reason }
                if crate::state::classify_prompt_completion(stop_reason)
                    == crate::state::PromptCompletion::Finished
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_review_path_preserves_legacy_encoding() {
        let old = r#"{"action":"request_review","params":{}}"#;
        let action: SubagentToolAction = serde_json::from_str(old).unwrap();
        assert_eq!(
            action,
            SubagentToolAction::RequestReview { paths: Vec::new() }
        );
        assert_eq!(serde_json::to_string(&action).unwrap(), old);
        // A request an older worker queued named one worktree.
        let queued = r#"{"action":"request_review","params":{"path":"/worktree"}}"#;
        assert_eq!(
            serde_json::from_str::<SubagentToolAction>(queued).unwrap(),
            SubagentToolAction::RequestReview {
                paths: vec!["/worktree".into()]
            }
        );
        let scoped = SubagentToolAction::RequestReview {
            paths: vec!["/worktree".into(), "/other".into()],
        };
        assert_eq!(
            serde_json::from_str::<SubagentToolAction>(&serde_json::to_string(&scoped).unwrap())
                .unwrap(),
            scoped
        );
    }

    // A request queued by an older worker carries one whole-verdict reason.
    #[test]
    fn a_queued_override_with_one_reason_decodes_as_one_pair() {
        let old = r#"{"action":"override_review","params":{"reason":"wrong premise"}}"#;
        let action: SubagentToolAction = serde_json::from_str(old).unwrap();
        assert_eq!(
            action,
            SubagentToolAction::OverrideReview {
                overrides: vec![ReviewOverride {
                    finding: LEGACY_WHOLE_VERDICT_FINDING.into(),
                    reason: "wrong premise".into(),
                }],
                path: None,
            }
        );
        let pairs = r#"{"action":"override_review","params":{"overrides":[{"finding":"F","reason":"R"}],"path":"/other"}}"#;
        let action: SubagentToolAction = serde_json::from_str(pairs).unwrap();
        assert_eq!(
            serde_json::from_str::<SubagentToolAction>(&serde_json::to_string(&action).unwrap())
                .unwrap(),
            action
        );
        assert!(
            matches!(&action, SubagentToolAction::OverrideReview { path: Some(path), .. } if path == "/other")
        );
    }

    #[test]
    fn only_child_mutations_hold_an_in_place_move_drain() {
        let mutations = [
            SubagentToolAction::Spawn {
                no_github_credentials: false,
                task_type: SubagentTaskType::Straightforward,
                task_name: "task".into(),
                instructions: "work".into(),

                working_directory: PathBuf::new(),
                context: None,
                files: Vec::new(),
            },
            SubagentToolAction::SendInput {
                child_session_id: "child".into(),
                message: "continue".into(),
            },
            SubagentToolAction::SendMessage {
                child_session_id: "child".into(),
                message: "legacy note".into(),
            },
            SubagentToolAction::SpawnOrchestrator {
                task_name: "task".into(),
                instructions: "work".into(),
                target: None,
                repos: None,
                no_github_credentials: false,
            },
            SubagentToolAction::CloseAgent {
                child_session_id: "child".into(),
            },
            SubagentToolAction::OverrideReview {
                overrides: vec![ReviewOverride {
                    finding: "finding".into(),
                    reason: "reason".into(),
                }],
                path: None,
            },
            SubagentToolAction::SetReviewBaseline {
                commit: "abc123".into(),
                path: None,
            },
        ];
        assert!(
            mutations
                .iter()
                .all(SubagentToolAction::mutates_child_state)
        );
        for observation in [
            SubagentToolAction::LegacyInterruptAgent {
                child_session_id: "child".into(),
            },
            SubagentToolAction::ListProfiles,
            SubagentToolAction::ListAgents,
            SubagentToolAction::LegacyWaitAgents,
            SubagentToolAction::LegacyAwaitChildren,
            SubagentToolAction::Handback {
                message: "done".into(),
            },
        ] {
            assert!(!observation.mutates_child_state());
        }
    }

    #[test]
    fn policies_preserve_legacy_records_but_public_policy_rejects_booleans() {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(
                default,
                alias = "mjolnir_subagents",
                deserialize_with = "deserialize_optional_policy"
            )]
            subagents: Option<SubagentPolicy>,
        }
        for (json, expected) in [
            (
                r#"{"mjolnir_subagents":true}"#,
                Some(SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                }),
            ),
            (
                r#"{"mjolnir_subagents":false}"#,
                Some(SubagentPolicy::Native),
            ),
            (
                r#"{"subagents":{"mode":"all_models"}}"#,
                Some(SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                }),
            ),
            (
                r#"{"subagents":{"mode":"single_model","model":"chosen","effort":"adaptive"}}"#,
                Some(SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                }),
            ),
            (
                r#"{"subagents":{"mode":"single_model","model":"chosen","effort":"high"}}"#,
                Some(SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                }),
            ),
            ("{}", None),
        ] {
            assert_eq!(
                serde_json::from_str::<Stored>(json).unwrap().subagents,
                expected
            );
        }
        assert!(serde_json::from_str::<SubagentPolicy>("true").is_err());
        let fixed = SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: Some(Box::new(crate::config::SubagentChoice {
                model: "gpt-6-luna".into(),
                effort: Some("max".into()),
            })),
            subtle: Some(Box::new(crate::config::SubagentChoice {
                model: "gpt-6-sol".into(),
                effort: None,
            })),
        };
        assert_eq!(
            serde_json::to_value(SubagentPolicy::Mjolnir {
                orchestrators: false,
                straightforward: None,
                subtle: None
            })
            .unwrap(),
            serde_json::json!({"mode":"mjolnir"})
        );
        assert_eq!(
            serde_json::to_value(&fixed).unwrap(),
            serde_json::json!({"mode":"mjolnir", "straightforward":{"model":"gpt-6-luna", "effort":"max"}, "subtle":{"model":"gpt-6-sol"}})
        );
        let default_effort = SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: Some(Box::new(crate::config::SubagentChoice {
                model: "gpt-6-luna".into(),
                effort: None,
            })),
            subtle: None,
        };
        assert_eq!(
            serde_json::to_value(&default_effort).unwrap(),
            serde_json::json!({"mode":"mjolnir", "straightforward":{"model":"gpt-6-luna"}})
        );
        for invalid in [
            serde_json::json!({"mode":"mjolnir", "model":"chosen", "effort":"adaptive"}),
            serde_json::json!({"mode":"mjolnir", "effort":"high"}),
            serde_json::json!({"mode":"mjolnir", "subtle":{"model":"chosen", "effort":"adaptive"}}),
        ] {
            assert!(serde_json::from_value::<SubagentPolicy>(invalid).is_err());
        }
        let old_flat: SubagentPolicy = serde_json::from_value(serde_json::json!({
            "mode":"mjolnir", "model":"gpt-6-luna", "effort":"max"
        }))
        .unwrap();
        let duplicated = crate::config::SubagentChoice {
            model: "gpt-6-luna".into(),
            effort: Some("max".into()),
        };
        assert_eq!(
            old_flat,
            SubagentPolicy::Mjolnir {
                orchestrators: false,
                straightforward: Some(Box::new(duplicated.clone())),
                subtle: Some(Box::new(duplicated))
            }
        );
        for harness in [
            crate::config::HarnessKind::Claude,
            crate::config::HarnessKind::Codex,
            crate::config::HarnessKind::Dsh,
            crate::config::HarnessKind::OpenCode,
        ] {
            let worker = SubagentRecord {
                resume_with_parent: false,
                kind: SubagentKind::Worker,
                reviewer_launch: None,
                child_session_id: "child".into(),
                parent_session_id: "parent".into(),
                task_name: "task".into(),
                profile_id: "codex".into(),
                model: None,
                effort: None,
                working_directory: PathBuf::new(),
                initial_prompt: String::new(),
                request_key: "req".into(),
                created_at: String::new(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: true,
                orchestrator: false,
            };
            let mut orchestrator = worker.clone();
            orchestrator.orchestrator = true;
            let enabled = SubagentPolicy::Mjolnir {
                orchestrators: true,
                straightforward: None,
                subtle: None,
            };
            assert_eq!(enabled.for_launch(harness, None), enabled);
            assert!(matches!(
                enabled.for_launch(harness, Some(&orchestrator)),
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    ..
                }
            ));
            for policy in [
                SubagentPolicy::Native,
                SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None,
                },
                fixed.clone(),
                default_effort.clone(),
                SubagentPolicy::None,
            ] {
                assert_eq!(
                    policy.for_launch(harness, Some(&worker)),
                    SubagentPolicy::None
                );
                assert_eq!(policy.for_launch(harness, Some(&orchestrator)), policy);
                assert_eq!(policy.for_launch(harness, None), policy);
            }
            // Depth-2 invariant: top-level -> orchestrator ok; orchestrator -> worker ok;
            // orchestrator -> orchestrator/oracle refused; worker -> worker refused.
            assert!(validate_subagent_parent(None, true, SubagentKind::Worker, true).is_ok());
            assert!(
                validate_subagent_parent(Some(&orchestrator), false, SubagentKind::Worker, false)
                    .is_ok()
            );
            assert!(
                validate_subagent_parent(Some(&orchestrator), false, SubagentKind::Worker, true)
                    .is_err()
            );
            assert!(
                validate_subagent_parent(Some(&orchestrator), false, SubagentKind::Oracle, false)
                    .is_err()
            );
            assert!(
                validate_subagent_parent(Some(&worker), false, SubagentKind::Worker, false)
                    .is_err()
            );
            assert!(
                validate_subagent_parent(Some(&worker), false, SubagentKind::Reviewer, false)
                    .is_ok()
            );
            // Keep existing serde round-trip check below for completeness.
            for policy in [SubagentPolicy::Native, SubagentPolicy::None] {
                assert_eq!(
                    serde_json::from_value::<SubagentPolicy>(
                        serde_json::to_value(&policy).unwrap()
                    )
                    .unwrap(),
                    policy
                );
            }
        }
    }

    fn finished(command_id: &str, stop_reason: &str) -> crate::state::MaterializedTurnOutcome {
        crate::state::MaterializedTurnOutcome {
            work_facts: None,
            answered_through: None,
            diagnostic: None,
            usage: None,
            command_id: command_id.to_owned(),
            accepted_ordinal: Some(1),
            turn_start_position: Some(1),
            completed_ordinal: 2,
            completed_at_ms: 10,
            outcome: crate::state::TurnOutcomeKind::Completed {
                stop_reason: stop_reason.to_owned(),
            },
        }
    }

    fn handback(command_id: &str) -> Option<SubagentHandback> {
        Some(SubagentHandback {
            command_id: command_id.to_owned(),
            message: "the report".to_owned(),
            recorded_at_ms: 5,
        })
    }

    fn reminder(for_command_id: &str) -> Option<HandbackReminder> {
        Some(HandbackReminder {
            command_id: "handback-reminder-r".to_owned(),
            for_command_id: for_command_id.to_owned(),
            sent_at_ms: 1_000,
        })
    }

    #[test]
    fn a_child_without_the_tool_or_a_turn_reports_its_last_message() {
        let turn = finished("task", "end_turn");
        let report = SubagentReport::default();
        assert_eq!(
            report_state(false, &report, Some(&turn), &Liveness::Known(None), &[], 0),
            ReportState::Fallback
        );
        assert_eq!(
            report_state(true, &report, None, &Liveness::Known(None), &[], 0),
            ReportState::Fallback
        );
    }

    // Hard-won: ebb2e9611: a handback recorded before restart lost its completed turn.
    #[test]
    fn a_handback_for_the_last_turn_is_the_report_whatever_the_turn_did() {
        let report = SubagentReport {
            handback: handback("task"),
            ..SubagentReport::default()
        };
        let recovered = crate::continuation::restart_continuation_id("task");
        assert!(report.handback.as_ref().unwrap().answers(&recovered));
        assert_eq!(
            report_state(
                true,
                &report,
                Some(&finished(&recovered, "end_turn")),
                &Liveness::Known(None),
                &[],
                0
            ),
            ReportState::Delivered("the report".into())
        );
        for stop_reason in ["end_turn", "cancelled", "refusal"] {
            assert_eq!(
                report_state(
                    true,
                    &report,
                    Some(&finished("task", stop_reason)),
                    &Liveness::Known(None),
                    &[],
                    0
                ),
                ReportState::Delivered("the report".to_owned()),
                "{stop_reason}"
            );
        }
        // A report for an earlier task does not answer this one.
        assert_eq!(
            report_state(
                true,
                &report,
                Some(&finished("next", "end_turn")),
                &Liveness::Known(None),
                &[],
                0
            ),
            ReportState::Pending { remind: true }
        );
    }

    #[test]
    fn only_a_normally_finished_task_turn_is_owed_a_reminder() {
        let report = SubagentReport::default();
        assert_eq!(
            report_state(
                true,
                &report,
                Some(&finished("task", "end_turn")),
                &Liveness::Known(None),
                &[],
                0
            ),
            ReportState::Pending { remind: true }
        );
        for stop_reason in ["cancelled", "awaiting_input", "quota_limit", "max_tokens"] {
            assert_eq!(
                report_state(
                    true,
                    &report,
                    Some(&finished("task", stop_reason)),
                    &Liveness::Known(None),
                    &[],
                    0
                ),
                ReportState::Fallback,
                "{stop_reason}"
            );
        }
        let mut interrupted = finished("task", "end_turn");
        interrupted.outcome = crate::state::TurnOutcomeKind::Interrupted {
            reason: None,
            message: "stopped".to_owned(),
        };
        assert_eq!(
            report_state(
                true,
                &report,
                Some(&interrupted),
                &Liveness::Known(None),
                &[],
                0
            ),
            ReportState::Fallback
        );
    }

    #[test]
    fn a_child_gets_one_reminder_and_then_reports_its_last_message() {
        let reminded = SubagentReport {
            reminder: reminder("task"),
            ..SubagentReport::default()
        };
        let task = finished("task", "end_turn");
        // Sent and queued, or sent a moment ago: it is on its way.
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&task),
                &Liveness::Known(None),
                &["handback-reminder-r"],
                60_000
            ),
            ReportState::Pending { remind: false }
        );
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&task),
                &Liveness::Known(None),
                &[],
                1_000 + HANDBACK_REMINDER_GRACE_MS - 1
            ),
            ReportState::Pending { remind: false }
        );
        // Neither queued, running nor finished long after it was sent.
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&task),
                &Liveness::Known(None),
                &[],
                1_000 + HANDBACK_REMINDER_GRACE_MS
            ),
            ReportState::Fallback
        );
        // The reminder turn itself ended without a report.
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&finished("handback-reminder-r", "end_turn")),
                &Liveness::Known(None),
                &[],
                2_000
            ),
            ReportState::Fallback
        );
        // A reminder that could not be sent.
        let failed = SubagentReport {
            reminder_failed_for: Some("task".to_owned()),
            ..SubagentReport::default()
        };
        assert_eq!(
            report_state(true, &failed, Some(&task), &Liveness::Known(None), &[], 0),
            ReportState::Fallback
        );
    }

    // Hard-won: 7c195cfd8: recovered reminders lost their reminder and give-up identity.
    #[test]
    fn a_reminder_turn_that_left_processes_running_holds_the_child() {
        for recovering in [false, true] {
            use crate::work_facts::WorkLiveness;
            let reminded = SubagentReport {
                reminder: reminder("task"),
                ..SubagentReport::default()
            };
            let reminder_id = if recovering {
                crate::continuation::restart_continuation_id(
                    &crate::continuation::restart_continuation_id("handback-reminder-r"),
                )
            } else {
                "handback-reminder-r".into()
            };
            let reminder_turn = finished(&reminder_id, "end_turn");
            let ordinal = reminder_turn.completed_ordinal;
            let live = |ordinal, alive| WorkLiveness {
                completed_ordinal: ordinal,
                alive,
                checked_at_ms: 0,
            };
            let known = |liveness| Liveness::Known(Some(liveness));
            let held = |now_ms, liveness: &Liveness| {
                report_state(true, &reminded, Some(&reminder_turn), liveness, &[], now_ms)
            };
            let give_up_at_ms = reminder_turn.completed_at_ms + HOLD_GIVE_UP_MS;
            let hold = |follow_up| ReportState::Held {
                since_ms: reminder_turn.completed_at_ms,
                give_up_at_ms,
                waiting_on: Vec::new(),
                follow_up,
            };
            assert_eq!(
                held(give_up_at_ms - 1, &known(live(ordinal, 2))),
                hold(None)
            );
            assert_eq!(
                held(give_up_at_ms, &known(live(ordinal, 2))),
                hold(Some(HoldFollowUpKind::GiveUp))
            );
            assert_eq!(
                held(5, &known(live(ordinal, 0))),
                hold(Some(HoldFollowUpKind::ProcessesExited))
            );
            // A worker that publishes nothing, or liveness of another turn, never holds.
            assert_eq!(held(5, &Liveness::Known(None)), ReportState::Fallback);
            assert_eq!(held(5, &known(live(ordinal + 1, 2))), ReportState::Fallback);
            // A reader with no worker to ask cannot say the child is not waiting.
            assert_eq!(
                held(5, &Liveness::Unknown),
                ReportState::Pending { remind: false }
            );
            // A follow-up already on its way is pending, not due again.
            let wake = hold_follow_up_command_id(HoldFollowUpKind::ProcessesExited, ordinal);
            assert_eq!(
                report_state(
                    true,
                    &reminded,
                    Some(&reminder_turn),
                    &known(live(ordinal, 0)),
                    &[wake.as_str()],
                    5
                ),
                ReportState::Pending { remind: false }
            );
            // The give-up turn ends the hold; a wake turn is an ordinary task turn
            // that owes a reminder, so the child can be held again.
            let give_up = hold_follow_up_command_id(HoldFollowUpKind::GiveUp, ordinal);
            let give_up = if recovering {
                crate::continuation::restart_continuation_id(&give_up)
            } else {
                give_up
            };
            assert_eq!(
                report_state(
                    true,
                    &reminded,
                    Some(&finished(&give_up, "end_turn")),
                    &Liveness::Known(None),
                    &[],
                    5
                ),
                ReportState::Fallback
            );
            assert_eq!(
                report_state(
                    true,
                    &reminded,
                    Some(&finished(&wake, "end_turn")),
                    &Liveness::Known(None),
                    &[],
                    5
                ),
                ReportState::Pending { remind: true }
            );
            assert_eq!(
                hold_follow_up_of(&wake),
                Some((HoldFollowUpKind::ProcessesExited, ordinal))
            );
            assert_eq!(
                hold_follow_up_of(&give_up),
                Some((HoldFollowUpKind::GiveUp, ordinal))
            );
            assert_eq!(
                finished(&give_up, "end_turn").answered_ordinal(),
                Some(ordinal)
            );
        }
    }

    // Hard-won: a91cfd04: A live wait reported a child completed 45 ms before its first turn and hid failures.
    #[test]
    fn a_child_is_not_done_until_a_finished_turn_reaches_the_newest_prompt() {
        let mut turn = finished("task", "end_turn");
        turn.accepted_ordinal = Some(20);
        assert!(!awaiting_prompt(None, Some(&turn)));
        assert!(awaiting_prompt(Some(45), None), "no turn has finished yet");
        assert!(
            awaiting_prompt(Some(45), Some(&turn)),
            "the finished turn is older"
        );
        // A prompt steered into the running turn is answered by that turn,
        // which then carries the steered prompt's ordinal.
        turn.accepted_ordinal = Some(45);
        assert!(!awaiting_prompt(Some(45), Some(&turn)));
    }

    // Hard-won: a91cfd04: A child failing on a usage limit looked completed and its parent never learned why.
    #[test]
    fn a_failed_turn_says_why_and_a_finished_one_did_not_fail() {
        assert_eq!(failed_turn(&finished("t", "end_turn"), Some("done")), None);
        assert_eq!(
            failed_turn(&finished("t", "awaiting_input"), Some("?")),
            None
        );
        let mut error = finished("t", "error");
        assert_eq!(
            failed_turn(&error, Some("You've hit your usage limit.")),
            Some(("failed", "You've hit your usage limit.".to_owned()))
        );
        error.diagnostic = Some(crate::diagnostic::TurnDiagnostic {
            message: "usageLimitExceeded".into(),
            code: None,
            http_status: None,
            reset_at: None,
            login_fingerprint: None,
        });
        assert_eq!(
            failed_turn(&error, Some("You've hit your usage limit.")),
            Some(("failed", "usageLimitExceeded".to_owned()))
        );
        assert_eq!(
            failed_turn(&finished("t", "cancelled"), None),
            Some(("interrupted", "the turn was cancelled".to_owned()))
        );
    }

    /// #1160: a child whose profile could not sign in died on its first
    /// request, and its parent read the error as the child's report. The
    /// turns below are the shapes that failure takes.
    // Hard-won: 1b077959: A live Codex login refusal looked like task output and caused repeated failed spawns.
    #[test]
    fn a_turn_the_provider_refused_for_its_login_is_a_login_failure() {
        let diagnostic = |message: &str, code: Option<&str>| crate::diagnostic::TurnDiagnostic {
            message: message.into(),
            code: code.map(str::to_owned),
            http_status: None,
            reset_at: None,
            login_fingerprint: None,
        };
        let failed_with = |message: &str, code: Option<&str>| {
            let mut turn = finished("t", "error");
            turn.diagnostic = Some(diagnostic(message, code));
            turn
        };
        // Codex could not refresh its ChatGPT login (R14-1).
        assert!(turn_failed_on_login(&failed_with(
            "Your access token could not be refreshed. Please log out and sign in again.",
            Some("unauthorized"),
        )));
        // Codex sent an API key the backend rejected.
        assert!(turn_failed_on_login(&failed_with(
            "unexpected status 401 Unauthorized: Incorrect API key provided: sk-svca****fvMA.",
            Some("-32603"),
        )));
        // The error kind alone is enough.
        assert!(turn_failed_on_login(&failed_with(
            "Your session ended.",
            Some("unauthorized")
        )));

        // A usage limit, another failure, and a turn that finished are not.
        assert!(!turn_failed_on_login(&failed_with(
            "You've hit your usage limit.",
            Some("usageLimitExceeded"),
        )));
        assert!(!turn_failed_on_login(&failed_with(
            "stream disconnected before completion",
            Some("responseStreamDisconnected"),
        )));
        assert!(!turn_failed_on_login(&finished("t", "end_turn")));
        assert!(!turn_failed_on_login(&finished("t", "cancelled")));

        assert_eq!(
            login_invalid_reason("codex4"),
            "the login is no longer valid; run `mj login --profile codex4` and spawn again"
        );
    }

    #[test]
    fn a_record_without_the_tool_flag_reads_as_having_no_tool() {
        let record: SubagentRecord = serde_json::from_str(
            r#"{"child_session_id":"c","parent_session_id":"p","task_name":"t","profile_id":"pr","working_directory":".","initial_prompt":"i","request_key":"k","created_at":"2026-09-15"}"#,
        )
        .expect("records written before the tool existed remain readable");
        assert!(!record.handback_tool);
        let encoded = serde_json::to_value(&record).expect("record encodes");
        assert!(encoded.get("handback_tool").is_none(), "{encoded}");
    }

    #[test]
    fn noticed_turn_keeps_the_stored_delivered_turn_field_name() {
        let record: SubagentRecord = serde_json::from_str(
            r#"{"child_session_id":"c","parent_session_id":"p","task_name":"t","profile_id":"pr","working_directory":".","initial_prompt":"i","request_key":"k","created_at":"2026-09-15","delivered_turn":3}"#,
        )
        .expect("stored relation payloads remain readable");
        assert_eq!(record.noticed_turn, Some(3));
        let encoded = serde_json::to_value(&record).expect("record encodes");
        assert_eq!(encoded["delivered_turn"], 3);
        assert!(
            encoded.get("noticed_turn").is_none(),
            "the wire field name must stay historical: {encoded}"
        );
    }

    #[test]
    fn terminal_finish_identity_ignores_legacy_timestamp_and_tracks_completed_turns() {
        let old: SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "state": "error",
            "detail": "startup failed",
            "updated_at": "2026-10-06T12:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            old,
            SubagentFinishIdentity::Terminal {
                state: "error".into(),
                detail: Some("startup failed".into()),
                last_completed_ordinal: None,
            }
        );

        let before: SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "state": "error",
            "detail": "startup failed",
            "last_completed_ordinal": 8
        }))
        .unwrap();
        let after: SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "state": "error",
            "detail": "startup failed",
            "last_completed_ordinal": 9
        }))
        .unwrap();
        assert_ne!(before, after);
        let encoded = serde_json::to_value(&old).unwrap();
        assert!(encoded.get("updated_at").is_none());
    }

    #[test]
    fn review_tools_belong_to_the_review_server_and_no_mj_agents_role() {
        assert_eq!(REVIEW_MCP_SERVER, "mj-review");
        assert_eq!(
            REVIEW_MCP_TOOLS,
            ["request_review", "override_review", "set_baseline"]
        );
        for role in [
            SubagentMcpRole::Parent,
            SubagentMcpRole::MessageOnly,
            SubagentMcpRole::FetchOnly,
            SubagentMcpRole::Child,
        ] {
            for tool in REVIEW_MCP_TOOLS {
                assert!(
                    !role.tool_names(true, true).contains(tool),
                    "{role} lists {tool}"
                );
            }
        }
    }

    #[test]
    fn an_old_interrupt_request_remains_readable_but_is_not_an_offered_tool() {
        let old: SubagentToolRequest = serde_json::from_str(
            r#"{"request_id":"old-interrupt","created_at_ms":1,"action":{"action":"interrupt_agent","params":{"child_session_id":"child"}}}"#,
        )
        .unwrap();
        assert_eq!(
            old.action,
            SubagentToolAction::LegacyInterruptAgent {
                child_session_id: "child".into()
            }
        );
        assert!(
            !SubagentMcpRole::Parent
                .tool_names(true, true)
                .contains(&"interrupt")
        );
    }

    #[test]
    fn a_persisted_send_message_request_remains_readable() {
        let old: SubagentToolRequest = serde_json::from_str(
            r#"{"request_id":"old-message","created_at_ms":1,"action":{"action":"send_message","params":{"child_session_id":"child","message":"continue"}}}"#,
        )
        .unwrap();
        assert_eq!(
            old.action,
            SubagentToolAction::SendMessage {
                child_session_id: "child".into(),
                message: "continue".into(),
            }
        );
        assert_eq!(
            serde_json::to_value(old).unwrap()["action"],
            serde_json::json!({
                "action":"send_message",
                "params":{"child_session_id":"child","message":"continue"}
            })
        );
        let cached_alias: SubagentToolRequest = serde_json::from_str(
            r#"{"request_id":"cached-input","created_at_ms":2,"action":{"action":"send_input","params":{"child_session_id":"child","message":"continue too"}}}"#,
        )
        .unwrap();
        assert_eq!(
            cached_alias.action,
            SubagentToolAction::SendInput {
                child_session_id: "child".into(),
                message: "continue too".into(),
            }
        );
        assert!(
            SubagentMcpRole::Parent
                .tool_names(true, true)
                .contains(&"send_message")
        );
        assert!(
            !SubagentMcpRole::Parent
                .tool_names(true, true)
                .contains(&"send_input")
        );
        assert!(
            !SubagentMcpRole::Parent
                .tool_names(true, true)
                .contains(&"interrupt")
        );
    }

    #[test]
    fn oracle_question_is_a_mutating_durable_tool_action() {
        let action = SubagentToolAction::AskOracle {
            message: "Choose B?".into(),
        };
        assert!(action.mutates_child_state());
        let wire = serde_json::to_value(&action).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"action":"ask_oracle","params":{"message":"Choose B?"}})
        );
        assert_eq!(
            serde_json::from_value::<SubagentToolAction>(wire).unwrap(),
            action
        );
    }

    #[test]
    fn wait_serializes_without_params_and_old_shapes_deserialize() {
        let wait = SubagentToolAction::LegacyWaitAgents;
        let encoded = serde_json::to_value(&wait).unwrap();
        assert_eq!(encoded, serde_json::json!({"action":"wait_agents"}));
        assert_eq!(
            serde_json::from_str::<SubagentToolAction>(r#"{"action":"await_children"}"#).unwrap(),
            SubagentToolAction::LegacyAwaitChildren
        );
        for role in [SubagentMcpRole::Parent, SubagentMcpRole::Child] {
            assert!(!role.tool_names(true, true).contains(&"wait"));
            assert!(!role.tool_names(true, true).contains(&"await_children"));
        }

        for old_shape in [
            r#"{"action":"wait_agents"}"#,
            r#"{"action":"wait_agents","params":{}}"#,
            r#"{"action":"wait_agents","params":{"child_session_ids":["c1"],"timeout_seconds":5,"return_when":"any"}}"#,
        ] {
            let decoded: SubagentToolAction = serde_json::from_str(old_shape).unwrap();
            assert_eq!(decoded, wait, "{old_shape}");
        }
    }

    #[test]
    fn a_wait_request_rejects_unknown_fields_after_retired_fields_are_stripped() {
        let error = serde_json::from_str::<SubagentToolAction>(
            r#"{"action":"wait_agents","params":{"unexpected":true}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("WaitAgents"), "{error}");
        let error = serde_json::from_str::<SubagentToolAction>(
            r#"{"action":"send_input","params":{"child_session_id":"c1","message":"x","unexpected":true}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected"), "{error}");
    }

    #[test]
    fn child_report_shares_fit_the_checkpoint_even_at_configuration_boundaries() {
        for concurrent in [0, 1, 6, 7, usize::MAX] {
            let limit = max_child_report_bytes(concurrent);
            assert!(
                u128::from(limit) * concurrent.max(1) as u128
                    <= u128::from(MAX_CHECKPOINT_REPORT_BYTES)
            );
        }
        assert_eq!(max_child_report_bytes(0), MAX_CHECKPOINT_REPORT_BYTES);
        assert_eq!(max_child_report_bytes(6), 11_184_810);
    }

    // Hard-won: cc7e6a2a: Real child reports of 27 to 62 KB were inlined into waits and triggered 21 repeated polls.
    #[test]
    fn a_long_report_is_cut_on_a_character_boundary_and_says_how_to_get_the_rest() {
        let short = "done".to_owned();
        assert_eq!(bounded_report(&short), (short.clone(), false));
        let exact = "é".repeat(MAX_HANDBACK_CHARS);
        assert_eq!(bounded_report(&exact), (exact.clone(), false));
        let long = "é".repeat(MAX_HANDBACK_CHARS + 25);
        let (cut, truncated) = bounded_report(&long);
        assert!(truncated);
        assert!(cut.starts_with(&exact));
        assert!(cut.contains("[truncated: 25 more characters"), "{cut}");
        assert!(cut.contains("send_message"), "{cut}");
    }

    #[test]
    fn a_task_summary_is_one_line_of_the_parents_own_instructions() {
        let report_dir = "/workspace/p/.mj-agents/c1";
        let prompt = format!(
            "{}\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>\n\n--- source \"a.rs\", lines 1-2 (one-based, inclusive) ---\nfn a() {{}}",
            handback_prompt_note(report_dir)
        );
        assert_eq!(
            task_summary(&prompt, Some(report_dir)).as_deref(),
            Some("Fix the parser. Keep the tests green.")
        );
        let assignment = format!(
            "{}\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>",
            report_directory_note(report_dir)
        );
        assert_eq!(
            task_summary(&assignment, Some(report_dir)).as_deref(),
            Some("Fix the parser. Keep the tests green.")
        );
        let consultation = format!(
            "{}\n\n{READ_ONLY_TASK_SCOPE}\n\nWhich option?",
            report_directory_note(report_dir)
        );
        assert_eq!(
            task_summary(&consultation, Some(report_dir)).as_deref(),
            Some("Which option?")
        );
        // Without the recorded directory the note is still recognised.
        assert_eq!(
            task_summary(&prompt, None).as_deref(),
            Some("Fix the parser. Keep the tests green.")
        );
        // Persisted prompts from before #1174 still expose the parent's task
        // after the injected note changes in a newer build.
        let old_prompt = format!(
            "You are a Mjolnir sub-agent. Finish every task by calling the mj-agents handback tool with your report: the session that started you reads only that report, not the rest of this conversation. Your report directory is {report_dir}. Your report is what the parent reads, and it is at most 4,000 characters: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Write anything longer (logs, tables, full findings) to files in your report directory and list their paths in the report; never put it in the report itself. Fix mechanical errors yourself (compile errors, lint and format findings, test failures your own change caused) and list each fix in one line of your report. Hand back a question and stop when the fix needs a design choice, touches a file you were not given, changes a persisted identity, a public contract or an epoch, or the failure also occurs on the base commit.\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>"
        );
        for directory in [Some(report_dir), None] {
            assert_eq!(
                task_summary(&old_prompt, directory).as_deref(),
                Some("Fix the parser. Keep the tests green.")
            );
        }
        // A child without the tool has no note before its instructions.
        assert_eq!(
            task_summary("Review the docs.", None).as_deref(),
            Some("Review the docs.")
        );
        assert_eq!(task_summary("  \n ", None), None);
        let long = "word ".repeat(100);
        let cut = task_summary(&long, None).unwrap();
        assert_eq!(cut.chars().count(), STOPPED_TASK_CHARS);
        assert!(cut.ends_with("word…"), "{cut}");
    }

    fn stopped(title: &str, task: Option<&str>, handed_back: bool) -> StoppedSubagent {
        StoppedSubagent {
            child_session_id: format!("id-{title}"),
            title: title.into(),
            task: task.map(str::to_owned),
            handed_back,
        }
    }

    #[test]
    fn in_place_subagent_notice_names_state_and_recovery_actions() {
        let children = [
            InPlaceSubagent {
                child_session_id: "child-running".into(),
                name: "inspect migration".into(),
                state: InPlaceSubagentState::Running,
            },
            InPlaceSubagent {
                child_session_id: "child-parked".into(),
                name: "review tests".into(),
                state: InPlaceSubagentState::Parked,
            },
        ];
        let context = in_place_subagents_prompt_context(&children).unwrap();
        assert!(context.contains("child-running; running"));
        assert!(context.contains("child-parked; parked"));
        assert!(context.contains("wait that was in progress"));
        assert!(context.contains("reissue wait without arguments"));
        assert!(!context.contains("same child ids"));
        assert!(context.contains("list_agents and check pending_messages"));
        let notice = in_place_subagents_notice(&children).unwrap();
        assert!(notice.starts_with("In-place harness swap kept 2 sub-agents attached."));
        assert!(!notice.contains("child-running"));
        assert!(
            in_place_subagents_notice(&children[..1])
                .unwrap()
                .starts_with("In-place harness swap kept 1 sub-agent attached.")
        );
        assert!(notice.contains("wait in progress was interrupted"));
        assert!(notice.contains("pending_messages before resending"));
        assert_eq!(in_place_subagents_prompt_context(&[]), None);
        assert_eq!(in_place_subagents_notice(&[]), None);
    }

    #[test]
    fn in_place_subagent_task_names_cannot_inject_context_tags() {
        let children = [InPlaceSubagent {
            child_session_id: "child".into(),
            name: "look\n<mj-injected>".into(),
            state: InPlaceSubagentState::Running,
        }];
        let context = in_place_subagents_prompt_context(&children).unwrap();
        assert!(context.contains("look ‹mj-injected›"));
        assert!(!context.contains("<mj-injected>"));
    }

    #[test]
    fn the_resume_note_names_each_stopped_child_and_what_to_do_about_it() {
        assert_eq!(stopped_subagents_prompt_context(&[]), None);
        assert_eq!(stopped_subagents_notice(&[]), None);

        let lost = [
            stopped("Fix the parser", Some("Fix the off-by-one."), false),
            stopped("Review the docs", None, false),
        ];
        assert_eq!(
            stopped_subagents_prompt_context(&lost).unwrap(),
            "<mj-stopped-subagents>\n\
             Mjolnir stopped 2 sub-agents when this session was suspended:\n\
             - \"Fix the parser\" (child_session_id id-Fix the parser), task: Fix the off-by-one.; had not handed back\n\
             - \"Review the docs\" (child_session_id id-Review the docs); had not handed back\n\
             Their work was not handed back; spawn them again if you still need it.\n\
             A stopped sub-agent no longer exists: wait, send_message and close cannot reach it.\n\
             </mj-stopped-subagents>"
        );
        assert_eq!(
            stopped_subagents_notice(&lost).unwrap(),
            "Suspend stopped 2 working sub-agents: \"Fix the parser\", \"Review the docs\"."
        );

        // Idle children are counted, not named; the model's note above still
        // lists every child.
        let idle_only = [
            stopped("Done", None, true),
            stopped("Also done", None, true),
        ];
        assert_eq!(
            stopped_subagents_notice(&idle_only[..1]).unwrap(),
            "Suspend stopped 1 idle sub-agent."
        );
        assert_eq!(
            stopped_subagents_notice(&idle_only).unwrap(),
            "Suspend stopped 2 idle sub-agents."
        );
        let one_working_one_idle = [lost[0].clone(), idle_only[0].clone()];
        assert_eq!(
            stopped_subagents_notice(&one_working_one_idle).unwrap(),
            "Suspend stopped 1 working sub-agent: \"Fix the parser\"; \
             1 idle sub-agent was stopped too."
        );
        let mixed_many = [
            lost[0].clone(),
            lost[1].clone(),
            idle_only[0].clone(),
            idle_only[1].clone(),
        ];
        assert_eq!(
            stopped_subagents_notice(&mixed_many).unwrap(),
            "Suspend stopped 2 working sub-agents: \"Fix the parser\", \"Review the docs\"; \
             2 idle sub-agents were stopped too."
        );

        let one = stopped_subagents_prompt_context(&lost[..1]).unwrap();
        assert!(
            one.contains("Mjolnir stopped 1 sub-agent when")
                && one.contains("Its work was not handed back; spawn it again"),
            "{one}"
        );
        let mixed = [lost[0].clone(), stopped("Done", None, true)];
        let note = stopped_subagents_prompt_context(&mixed).unwrap();
        assert!(note.contains("\"Done\" (child_session_id id-Done); had handed back"));
        assert!(note.contains("The work of those that had not handed back was lost"));
        let finished = stopped_subagents_prompt_context(&mixed[1..]).unwrap();
        assert!(
            finished.contains("It had handed back its report"),
            "{finished}"
        );
        // A reserved block, so it never shows in a harness's own copy of the prompt.
        assert_eq!(
            crate::relay::strip_hidden_prompt_context(&format!("{note}\nuser text")),
            "user text"
        );
    }

    #[test]
    fn a_stopped_oracle_is_one_line_with_no_id_and_no_prompt() {
        let oracle = StoppedSubagent::oracle();
        assert_eq!(oracle.task, None);
        // Alone, it is only the line.
        assert_eq!(
            stopped_subagents_prompt_context(std::slice::from_ref(&oracle)).unwrap(),
            format!("<mj-stopped-subagents>\n{STOPPED_ORACLE_LINE}\n</mj-stopped-subagents>")
        );
        assert_eq!(
            stopped_subagents_notice(std::slice::from_ref(&oracle)).unwrap(),
            STOPPED_ORACLE_LINE
        );
        // Beside children, it is neither counted nor listed as one.
        let list = [stopped("Fix the parser", None, false), oracle];
        let note = stopped_subagents_prompt_context(&list).unwrap();
        assert!(note.contains("Mjolnir stopped 1 sub-agent when"), "{note}");
        assert!(note.contains(STOPPED_ORACLE_LINE), "{note}");
        assert!(!note.contains("child_session_id oracle"), "{note}");
        assert_eq!(
            stopped_subagents_notice(&list).unwrap(),
            format!(
                "Suspend stopped 1 working sub-agent: \"Fix the parser\". {STOPPED_ORACLE_LINE}"
            )
        );
    }

    // Hard-won: 1349c768: Suspend confirmation counted unfinished children but did not identify which children would stop.
    #[test]
    fn the_suspend_confirmation_names_up_to_three_children_and_counts_the_rest() {
        let titles = |count: usize| {
            ["Alpha", "Bravo", "Charlie", "Delta", "Echo"][..count]
                .iter()
                .map(|title| (*title).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(suspend_warning_naming(&titles(0)), None);
        assert_eq!(
            suspend_warning_naming(&titles(1)).as_deref(),
            Some("Sub-agent \"Alpha\" has not handed back; suspending stops it")
        );
        assert_eq!(
            suspend_warning_naming(&titles(2)).as_deref(),
            Some("Sub-agents \"Alpha\" and \"Bravo\" have not handed back; suspending stops them")
        );
        assert_eq!(
            suspend_warning_naming(&titles(3)).as_deref(),
            Some(
                "Sub-agents \"Alpha\", \"Bravo\" and \"Charlie\" have not handed back; \
                 suspending stops them"
            )
        );
        assert_eq!(
            suspend_warning_naming(&titles(4)).as_deref(),
            Some(
                "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 1 more have not handed \
                 back; suspending stops them"
            )
        );
        assert_eq!(
            suspend_warning_naming(&titles(5)).as_deref(),
            Some(
                "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 2 more have not handed \
                 back; suspending stops them"
            )
        );
    }

    /// A report handed back in a reminder turn answers the prompts accepted
    /// before the reminded turn finished, though the reminder turn has no
    /// acceptance ordinal of its own (I1-3, I1-4).
    // Hard-won: e1207055: Two live waits parked forever when the child's report arrived during a reminder turn.
    #[test]
    fn a_report_handed_back_in_a_reminder_turn_answers_the_reminded_prompt() {
        let reminder_id = handback_reminder_command_id(103);
        assert_eq!(reminded_turn_ordinal(&reminder_id), Some(103));
        assert_eq!(reminded_turn_ordinal("subagent-input-1"), None);
        let reminder_turn = crate::state::MaterializedTurnOutcome {
            accepted_ordinal: None,
            turn_start_position: None,
            completed_ordinal: 118,
            ..finished(&reminder_id, "end_turn")
        };
        let report = SubagentReport {
            handback: handback(&reminder_id),
            awaited_ordinal: Some(84),
            ..SubagentReport::default()
        };
        assert!(!awaiting_prompt(Some(84), Some(&reminder_turn)));
        assert!(has_handed_back(
            true,
            &report,
            false,
            Some(&reminder_turn),
            &Liveness::Known(None),
            0
        ));
        let harness_turn = crate::state::MaterializedTurnOutcome {
            command_id: crate::continuation::harness_turn_id(103),
            turn_start_position: Some(103),
            answered_through: Some(83),
            ..reminder_turn.clone()
        };
        // Hard-won: #1450 review: a queued task must prevent handback.
        let queued_report = SubagentReport {
            awaited_ordinal: Some(84),
            ..Default::default()
        };
        assert!(!has_handed_back(
            true,
            &queued_report,
            false,
            Some(&harness_turn),
            &Liveness::Known(None),
            0
        ));
        assert!(awaiting_prompt(Some(84), Some(&harness_turn)));
        assert!(!awaiting_prompt(Some(83), Some(&harness_turn)));
        assert!(awaiting_prompt(Some(110), Some(&harness_turn)));
        // A prompt accepted after the reminded turn ended is still owed.
        assert!(awaiting_prompt(Some(110), Some(&reminder_turn)));
    }

    #[test]
    fn a_child_has_handed_back_only_when_its_final_report_answers_the_newest_prompt() {
        let task = finished("task", "end_turn");
        let delivered = SubagentReport {
            handback: handback("task"),
            ..SubagentReport::default()
        };
        assert!(has_handed_back(
            true,
            &delivered,
            false,
            Some(&task),
            &Liveness::Known(None),
            0
        ));
        // Still working, never finished a turn, or given a newer prompt.
        assert!(!has_handed_back(
            true,
            &delivered,
            true,
            Some(&task),
            &Liveness::Known(None),
            0
        ));
        assert!(!has_handed_back(
            true,
            &delivered,
            false,
            None,
            &Liveness::Known(None),
            0
        ));
        let newer = SubagentReport {
            awaited_ordinal: Some(9),
            ..delivered.clone()
        };
        assert!(!has_handed_back(
            true,
            &newer,
            false,
            Some(&task),
            &Liveness::Known(None),
            0
        ));
        // Owes a report and has not been reminded, or was reminded just now.
        assert!(!has_handed_back(
            true,
            &SubagentReport::default(),
            false,
            Some(&task),
            &Liveness::Known(None),
            0
        ));
        let reminded = SubagentReport {
            reminder: reminder("task"),
            ..SubagentReport::default()
        };
        assert!(!has_handed_back(
            true,
            &reminded,
            false,
            Some(&task),
            &Liveness::Known(None),
            1_000
        ));
        // Its last message is the report once the reminder went unanswered,
        // and always for a child without the tool.
        assert!(has_handed_back(
            true,
            &reminded,
            false,
            Some(&task),
            &Liveness::Known(None),
            1_000 + HANDBACK_REMINDER_GRACE_MS
        ));
        assert!(has_handed_back(
            false,
            &SubagentReport::default(),
            false,
            Some(&task),
            &Liveness::Known(None),
            0
        ));
        // A turn that failed or asked a question handed nothing back.
        for stop_reason in ["cancelled", "awaiting_input", "refusal"] {
            assert!(
                !has_handed_back(
                    false,
                    &SubagentReport::default(),
                    false,
                    Some(&finished("task", stop_reason)),
                    &Liveness::Known(None),
                    0
                ),
                "{stop_reason}"
            );
        }
    }
    #[test]
    fn delegation_policy_offers_a_task_type_only_when_subtle_is_configured() {
        let both = delegation_policy(4, true);
        assert!(
            both.contains("Set `task_type` to `straightforward`"),
            "{both}"
        );
        assert!(both.contains("for bounded work with a single clear path"));
        assert!(
            both.contains("for multi-file implementation (even from a settled design), multi-stage\noperational runs or troubleshooting"),
            "{both}"
        );
        assert!(both.contains("A subtle task gets a smarter, more\nexpensive agent."));
        assert!(
            both.contains(
                "When a `straightforward` child stalls or returns incomplete after one\ncorrection, escalate that slice to `subtle` rather than taking it over yourself."
            ),
            "{both}"
        );
        // With only straightforward, there is no choice: the parent delegates
        // straightforward work and keeps work that needs judgment.
        let one = delegation_policy(4, false);
        assert!(
            one.contains("`close` tools; Mjolnir supplies the configured\nchild model and effort. Delegate straightforward work: "),
            "{one}"
        );
        assert!(
            one.contains("Keep multi-file\nimplementation, troubleshooting, ambiguous diagnosis, design choices"),
            "{one}"
        );
        assert!(
            one.contains("Judge by scope and branching,\nnot brief length or command runtime.\nUp to 4 children"),
            "{one}"
        );
        // Hard-won: e105bd9ce: the single-type judgment rule contradicted a
        // closed list of reasons for keeping a slice. The policy now weighs
        // the cost of each piece of work instead of listing reasons.
        assert!(
            one.contains("Weigh each\npiece of work.") && !one.contains("only when it needs"),
            "{one}"
        );
        assert!(
            one.contains("Decide who builds and when, yourself included"),
            "{one}"
        );
        assert!(
            one.contains("delegate any resulting edits, test-fixture repairs, and merge conflicts that\nfit your configured children once they go beyond a few calls. Where a child\nalready owns that slice and matches its scope, prefer resuming it via\n`send_message`"),
            "{one}"
        );
        for text in ["task_type", "task type", "`subtle`", "$TASK_TYPES", "$N"] {
            assert!(!one.contains(text), "{text}: {one}");
        }
        assert!(!both.contains("$TASK_TYPES"));
    }

    #[test]
    fn durable_legacy_spawn_selectors_are_ignored_and_task_type_defaults() {
        let action: SubagentToolAction = serde_json::from_value(serde_json::json!({
            "action": "spawn", "params": { "task_name":"legacy", "instructions":"report",
                "profile_id":"old", "model":"old-model", "effort":"adaptive" }
        }))
        .unwrap();
        let SubagentToolAction::Spawn { task_type, .. } = &action else {
            panic!("spawn")
        };
        assert_eq!(*task_type, SubagentTaskType::Straightforward);
        let encoded = serde_json::to_value(&action).unwrap();
        for key in ["profile_id", "model", "effort"] {
            assert!(encoded["params"].get(key).is_none());
        }

        for spelling in ["supervisor", "orchestrator"] {
            let mut request = encoded.clone();
            request["params"]["task_type"] = spelling.into();
            let action: SubagentToolAction = serde_json::from_value(request).unwrap();
            assert!(matches!(
                action,
                SubagentToolAction::Spawn {
                    task_type: SubagentTaskType::Orchestrator,
                    ..
                }
            ));
            assert_eq!(
                serde_json::to_value(action).unwrap()["params"]["task_type"],
                "supervisor"
            );
            assert_eq!(
                spelling.parse::<SubagentMcpRole>().unwrap(),
                SubagentMcpRole::Orchestrator
            );
        }
        assert_eq!(
            serde_json::from_str::<SubagentMcpRole>("\"fixed_parent\"").unwrap(),
            SubagentMcpRole::Parent
        );
        assert_eq!(
            "fixed_parent".parse::<SubagentMcpRole>().unwrap(),
            SubagentMcpRole::Parent
        );
    }
}

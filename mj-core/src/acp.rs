//! Normalized ACP data and controls shared by workers and control surfaces.
pub mod claude_result;
#[doc(hidden)]
pub mod dialect;
pub mod step_clock;
pub mod surface;
mod terminal_compat;
use crate::elicitation::*;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::*;
pub use claude_result::{ClaudeResultUsage, ClaudeTurnResult};
use dialect::grok;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub use step_clock::StepClock;
pub use surface::PlanControl;
pub use terminal_compat::*;
/// Identity prefix every normalized plan decision shares, whatever harness
/// dialect produced it.
pub const PLAN_REVIEW_ID_PREFIX: &str = "plan-review-";

/// Header [`normalized_plan_review`] puts in front of the harness's proposal
/// text. Reading the proposal back out is the inverse, so both live here.
pub const PLAN_REVIEW_MESSAGE_PREFIX: &str = "Review the agent's plan:\n\n";

/// The plan decision Hel answers itself instead of forwarding to the harness.
/// Every other decision maps to a native option through the dialect bridge.
pub const PLAN_REVIEW_SECOND_OPINION: &str = "second_opinion";

pub const PLAN_REVIEW_ACTION: &str = "action";

pub const PLAN_REVIEW_FEEDBACK: &str = "feedback";

/// Marker Hel adds to the warning for a prompt the bridge failed with ACP's
/// `auth_required`. The wire message is a bare "Authentication required", too
/// generic for `credentials` to match on text alone, so the error code —
/// not the bridge's wording — decides whether the credential heuristic fires.
pub const PROMPT_AUTH_REQUIRED_MARKER: &str = "ACP auth_required";

/// Marker on a successful ACP response that carried no session updates. Some
/// bridges use this shape when their underlying turn failed, so completing it
/// silently would leave a user line with no answer or explanation.
pub const PROMPT_EMPTY_RESPONSE_MARKER: &str = "ACP prompt returned no session updates";

/// Identity of the one question Hel raises about a session's own stored
/// configuration.
///
/// It is constant because the projection dedupes pending elicitations by id:
/// a restart before the operator answers replaces the question instead of
/// stacking another copy of it.
pub const SESSION_CONFIG_RECOVERY_ID: &str = "session-config-recovery";

pub fn plan_review_carries_native_feedback(id: &str) -> bool {
    grok::is_plan_review_id(id)
}

/// Whether this elicitation id belongs to one of Hel's normalized plan
/// decisions.
#[must_use]
pub fn is_plan_review_id(id: &str) -> bool {
    id.starts_with(PLAN_REVIEW_ID_PREFIX)
}

/// The exact proposal text a normalized plan decision carries.
///
/// Returns `None` for any other elicitation, and for a plan decision whose
/// message was not built by [`normalized_plan_review`].
#[must_use]
pub fn plan_review_proposal(request: &ElicitationRequest) -> Option<&str> {
    if !is_plan_review_id(&request.id) {
        return None;
    }
    request.message.strip_prefix(PLAN_REVIEW_MESSAGE_PREFIX)
}

/// The proposal to review when this answer asked for a second opinion.
///
/// A second opinion is local: the harness's decision stays pending while Hel
/// sets the reviewer up, so this answer must never reach ACP. Callers use the
/// returned proposal as the captured text they hand to the reviewer.
#[must_use]
pub fn plan_review_second_opinion<'a>(
    request: &'a ElicitationRequest,
    response: &ElicitationResponse,
) -> Option<&'a str> {
    let proposal = plan_review_proposal(request)?;
    let ElicitationResponse::Accept { content } = response else {
        return None;
    };
    match content.get(PLAN_REVIEW_ACTION) {
        Some(ElicitationValue::String(action)) if action == PLAN_REVIEW_SECOND_OPINION => {
            Some(proposal)
        }
        _ => None,
    }
}

/// The answer Hel gives the harness once a second opinion has been set up.
///
/// Gathering context needs an idle planning session, so the pending decision
/// has to be resolved first. Declining keeps plan mode active, which is why
/// the captured proposal is the only copy of the plan that survives and why
/// cancelling a review owes the user a Hel-owned decision in its place.
#[must_use]
pub fn plan_review_keep_planning() -> ElicitationResponse {
    ElicitationResponse::Accept {
        content: std::collections::BTreeMap::from([(
            PLAN_REVIEW_ACTION.to_owned(),
            ElicitationValue::String("keep_planning".to_owned()),
        )]),
    }
}

pub fn normalized_plan_review(id: String, value: &serde_json::Value) -> ElicitationRequest {
    let plan = nested_string(value, &["plan", "plan_content", "planContent"])
        .unwrap_or("The agent did not provide plan text in its review request.");
    ElicitationRequest {
        id,
        title: Some("Plan review".into()),
        message: format!("{PLAN_REVIEW_MESSAGE_PREFIX}{plan}"),
        description: Some("Choose what Mjolnir should tell the planning harness.".into()),
        fields: vec![
            ElicitationField {
                id: PLAN_REVIEW_ACTION.into(),
                title: "Decision".into(),
                description: Some(
                    "Implement approves the plan; revise sends the feedback below.".into(),
                ),
                required: true,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::SingleSelect {
                    options: vec![
                        ElicitationOption {
                            value: "implement".into(),
                            title: "Implement".into(),
                            description: Some("Approve and continue with implementation".into()),
                            preview: None,
                        },
                        ElicitationOption {
                            value: "revise".into(),
                            title: "Revise".into(),
                            description: Some("Keep planning and incorporate feedback".into()),
                            preview: None,
                        },
                        ElicitationOption {
                            value: PLAN_REVIEW_SECOND_OPINION.into(),
                            title: "Get a second opinion".into(),
                            description: Some(
                                "Ask another agent to review this plan before you decide".into(),
                            ),
                            preview: None,
                        },
                        ElicitationOption {
                            value: "keep_planning".into(),
                            title: "Keep planning".into(),
                            description: Some("Decline this plan without leaving plan mode".into()),
                            preview: None,
                        },
                        ElicitationOption {
                            value: "exit".into(),
                            title: "Exit plan mode".into(),
                            description: Some(
                                "Abandon this review and return to normal mode".into(),
                            ),
                            preview: None,
                        },
                    ],
                    default: Some("keep_planning".into()),
                },
            },
            ElicitationField {
                id: PLAN_REVIEW_FEEDBACK.into(),
                title: "Revision feedback".into(),
                description: Some("Describe what the agent should change.".into()),
                required: false,
                secret: false,
                custom_answer_for: Some(PLAN_REVIEW_ACTION.into()),
                custom_answer_option: Some("revise".into()),
                kind: ElicitationFieldKind::Text {
                    default: None,
                    min_length: None,
                    max_length: Some(16 * 1024),
                    pattern: None,
                    format: None,
                },
            },
        ],
    }
}

pub fn nested_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(serde_json::Value::as_str) {
                    return Some(value);
                }
            }
            object.values().find_map(|value| nested_string(value, keys))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| nested_string(value, keys))
        }
        _ => None,
    }
}

pub fn session_config_choices(
    options: &[SessionConfigOption],
    key: &str,
) -> Vec<SessionConfigChoice> {
    let Some(option) = find_session_config_option(options, key) else {
        return Vec::new();
    };
    let SessionConfigKind::Select(select) = &option.kind else {
        return Vec::new();
    };
    let choices = match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect::<Vec<_>>(),
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|group| &group.options).collect()
        }
        _ => Vec::new(),
    };
    choices
        .into_iter()
        .map(|choice| SessionConfigChoice {
            value: choice.value.to_string(),
            name: choice.name.clone(),
            description: choice.description.clone(),
        })
        .collect()
}

/// `a` or `an` for a session-configuration key, so a sentence built from the
/// key's own name reads as English: "a model selector", "an effort selector".
pub fn config_key_article(key: &str) -> &'static str {
    if key.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

/// What to report when the harness advertises no selector for `key` at all.
/// The worker and the terminal say the same words, so the refusal a surface
/// shows the moment it knows and the one the transcript records agree.
pub fn missing_config_selector_refusal(key: &str) -> String {
    format!(
        "ACP bridge does not expose {} {key} selector",
        config_key_article(key)
    )
}

pub fn find_session_config_option<'a>(
    options: &'a [SessionConfigOption],
    key: &str,
) -> Option<&'a SessionConfigOption> {
    if let Some(option) = options.iter().find(|option| option.id.to_string() == key) {
        return Some(option);
    }
    match key {
        "model" => options.iter().find(|option| {
            option.category == Some(SessionConfigOptionCategory::Model)
                && !matches!(
                    option.id.to_string().as_str(),
                    "effort" | "reasoning_effort"
                )
        }),
        "effort" => options
            .iter()
            .find(|option| option.category == Some(SessionConfigOptionCategory::ThoughtLevel))
            .or_else(|| {
                options.iter().find(|option| {
                    matches!(
                        option.id.to_string().as_str(),
                        "effort" | "reasoning_effort"
                    )
                })
            }),
        "mode" => options
            .iter()
            .find(|option| option.category == Some(SessionConfigOptionCategory::Mode)),
        _ => None,
    }
}

pub fn select_contains(kind: &SessionConfigKind, desired: &str) -> bool {
    let SessionConfigKind::Select(select) = kind else {
        return false;
    };
    match &select.options {
        agent_client_protocol::schema::v1::SessionConfigSelectOptions::Ungrouped(options) => {
            options
                .iter()
                .any(|option| option.value.to_string() == desired)
        }
        agent_client_protocol::schema::v1::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| &group.options)
            .any(|option| option.value.to_string() == desired),
        _ => false,
    }
}

/// Only catalogue and selector announcements can prove a thread is unused.
/// Treat all other updates, including future ACP variants, as native history.
pub fn session_update_has_native_history(update: &SessionUpdate) -> bool {
    !matches!(
        update,
        SessionUpdate::AvailableCommandsUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::CurrentModeUpdate(_)
            // Mjolnir synthesizes this variant itself, in `goal::publish`
            // (`mj-worker/src/acp/goal.rs`), to carry its own goal metadata.
            // It never comes from the agent, so it is not agent content.
            | SessionUpdate::SessionInfoUpdate(_)
    )
}

/// The stop reason recorded for a turn the harness ended without answering.
///
/// A reason of its own, like `harness_inactive` for a stalled turn, so
/// `mj wait`, the session summary, the chat and the recorded events all name
/// what happened and automation can tell this apart from any other error. Any
/// stop reason that is not a known completion already classifies as an error
/// (`crate::state::classify_prompt_completion`), so nothing has to learn this
/// string to keep working.
pub const PROMPT_UNANSWERED_STOP_REASON: &str = "prompt_unanswered";

/// The classifier inferred a user handoff while the harness still held its prompt open.
pub const AWAITING_INPUT_STOP_REASON: &str = "awaiting_input";

/// The progress text an ACP bridge streams while it compacts a session's
/// context, verbatim from the bridges Mjolnir pins.
///
/// `@agentclientprotocol/claude-agent-acp` emits these as ordinary assistant
/// text (`dist/acp-agent.js`, the `status` handler keyed on `compacting` and
/// `compact_result`), so they are indistinguishable from an answer unless they
/// are named. Compaction is not in the ACP schema this build compiles against,
/// which is why there is nothing better to match on. See issue #970.
const COMPACTION_BANNERS: &[&str] = &[
    "compacting...",
    "compacting completed.",
    "compacting failed",
];

/// Whether this update is a bridge's own compaction progress text rather than
/// anything the agent produced by working.
///
/// A turn carrying only these compacted the context; it did not act on the
/// prompt. Recognizing them is what lets Mjolnir see a prompt that was
/// swallowed during compaction, which is the loss reported in #970.
pub fn session_update_is_compaction_banner(update: &SessionUpdate) -> bool {
    let chunk = match update {
        SessionUpdate::AgentMessageChunk(chunk) | SessionUpdate::AgentThoughtChunk(chunk) => chunk,
        _ => return false,
    };
    let ContentBlock::Text(text) = &chunk.content else {
        return false;
    };
    let text = text.text.trim().to_lowercase();
    COMPACTION_BANNERS
        .iter()
        .any(|banner| text.starts_with(banner))
}

/// Whether this prompt is Mjolnir forwarding a request to compact the context.
///
/// A turn that answers this prompt with nothing but compaction banners did
/// exactly what was asked, so it must not be reported as unanswered. Only
/// Mjolnir's own outgoing text is inspected, never the harness's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCommand {
    Clear,
    Compact,
}

/// Interpret only the user's first block, before attaching background context.
pub fn context_command(prompt: &[ContentBlock]) -> Option<(ContextCommand, &str)> {
    let ContentBlock::Text(first) = prompt.first()? else {
        return None;
    };
    context_command_text(&first.text)
}

pub fn context_command_text(text: &str) -> Option<(ContextCommand, &str)> {
    let text = text.trim().strip_prefix('/')?;
    let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let command = if name.eq_ignore_ascii_case("clear") {
        ContextCommand::Clear
    } else if name.eq_ignore_ascii_case("compact") {
        ContextCommand::Compact
    } else {
        return None;
    };
    Some((command, args.trim()))
}

pub fn prompt_requests_compaction(prompt: &[ContentBlock]) -> bool {
    matches!(context_command(prompt), Some((ContextCommand::Compact, _)))
}

/// Whether the prompt is a harness slash command such as `/review` rather
/// than a message. The command name is one word of letters, digits, `_`, `-`
/// or `:`, so a prompt that opens with a path like `/home/me/file` is a
/// message.
pub fn prompt_is_slash_command(prompt: &[ContentBlock]) -> bool {
    let Some(ContentBlock::Text(first)) = prompt.first() else {
        return false;
    };
    let Some(rest) = first.text.trim_start().strip_prefix('/') else {
        return false;
    };
    let name = rest.split(char::is_whitespace).next().unwrap_or_default();
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'))
}

/// Whether this update is the agent doing the work a prompt asked for.
///
/// This is how Mjolnir tells "the harness answered" from "the harness ended
/// the turn without acting on the prompt" (#970). Answering is deliberately
/// defined widely: a turn that only ran tools, edited files or executed
/// commands and never wrote a word has answered. Only traffic that arrives
/// without the agent having done anything is excluded — command catalogues,
/// mode and configuration announcements, session metadata, and usage
/// accounting, all of which a harness emits on its own schedule.
///
/// An unrecognized future variant counts as output. `SessionUpdate` is
/// `#[non_exhaustive]`, and reporting an answered turn as unanswered because
/// this build is older than the harness would be worse than missing a swallow.
pub fn session_update_is_agent_output(update: &SessionUpdate) -> bool {
    !matches!(
        update,
        SessionUpdate::AvailableCommandsUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_)
            | SessionUpdate::UsageUpdate(_)
    ) && !session_update_is_compaction_banner(update)
}

/// The stable part of Codex's refusal to resume a thread it never wrote to
/// disk. Codex defers a thread's rollout file until the first user message, so
/// a thread that was created and never prompted does not exist to resume.
/// codex-acp wraps the message, and it reaches Mjolnir looking like
/// `Internal error: {"details": "no rollout found for thread id 0199…"}`, so
/// the substring is what can be matched.
pub const CODEX_MISSING_THREAD_MESSAGE: &str = "no rollout found for thread id";

/// Newer codex-acp builds report the same never-written thread as
/// `thread not found: <thread id>`. It names the thread, so it only counts
/// for the thread being resumed.
pub const CODEX_THREAD_NOT_FOUND_MESSAGE: &str = "thread not found: ";

/// The stable part of Claude Code's refusal to resume a session it never wrote
/// a transcript for. Claude Code writes a session's transcript file at the
/// first prompt, so a session that was opened and never prompted does not
/// exist to resume. `@agentclientprotocol/claude-agent-acp` answers the ACP
/// standard "resource not found" error, and it reaches Mjolnir as
/// `Resource not found: <session id>: {"uri": "<session id>"}`. The session id
/// is matched along with the prefix, so an unrelated missing resource can
/// never be read as a missing session.
pub const CLAUDE_MISSING_SESSION_MESSAGE: &str = "Resource not found: ";

/// Whether a failed `session/resume` or `session/load` says the harness has no
/// such native session. Only harnesses that defer writing a session to disk
/// until its first user message can report a session Mjolnir believes it
/// created; every other harness answers `false`, so its reload failure keeps
/// failing loudly. If a harness rewords its message, this stops matching and
/// the resume fails loudly again; it can never degrade into silently replacing
/// a native session that still exists.
pub fn error_reports_missing_native_session(
    harness: HarnessKind,
    native_session_id: &str,
    error: &str,
) -> bool {
    match harness {
        HarnessKind::Codex => {
            error.contains(CODEX_MISSING_THREAD_MESSAGE)
                || error.contains(&format!(
                    "{CODEX_THREAD_NOT_FOUND_MESSAGE}{native_session_id}"
                ))
        }
        HarnessKind::Claude => error.contains(&format!(
            "{CLAUDE_MISSING_SESSION_MESSAGE}{native_session_id}"
        )),
        HarnessKind::Kimi | HarnessKind::Grok | HarnessKind::Muse => false,
    }
}

/// Only model and reasoning effort survive a bridge replacement. Restoring
/// plan/permission modes here could override the current execution policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedSessionConfig {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// A task in Claude Code's process-local background-task level signal.
///
/// The adapter also sends `task_type` and an optional `ambient` marker. Hel
/// only needs the stable id and user-facing description, and filters ambient
/// housekeeping tasks before publishing the replacement level to the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeBackgroundTask {
    pub task_id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    NativeAgent {
        event: crate::native_agent::NativeAgentEvent,
    },
    ContinuationExpected {
        since_ms: i64,
        note: String,
        generation: u64,
    },
    Connected {
        agent_name: Option<String>,
        agent_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol_version: Option<ProtocolVersion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Box<AgentCapabilities>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_info: Option<Implementation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steering_supported: Option<bool>,
        /// The bridge hands a steer back when no turn can take it, instead of
        /// starting a turn of its own, so queued prompts may be steered
        /// without the user asking.
        #[serde(default)]
        steering_returns_idle_input: bool,
    },
    ContextClearing {
        request_id: String,
    },
    ContextCleared {
        request_id: String,
        native_session_id: String,
        memory: Option<String>,
    },
    SessionStarted {
        native_session_id: String,
        resumed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_mode: Option<String>,
        /// This session opened fresh because the recorded native session
        /// could not be reloaded and the harness keeps no native state a
        /// checkpoint could restore. Older workers omit it.
        #[serde(default)]
        native_continuity_lost: bool,
    },
    SessionConfigured {
        config_options: Vec<SessionConfigOption>,
    },
    /// The native thread now holds something only it can replay: the agent
    /// sent conversation content, or a prompt was transmitted to it. The
    /// worker persists this so a later restart never treats the thread as an
    /// empty one it may replace. Emitted once per bridge life, on the change.
    NativeSessionUsed,
    SessionModesConfigured {
        modes: Option<SessionModeState>,
    },
    SessionUpdate {
        update: serde_json::Value,
    },
    /// Replacement level for Claude Code's live non-ambient background tasks.
    /// This provider signal does not represent transcript or foreground work.
    ClaudeBackgroundTasksChanged {
        tasks: Vec<ClaudeBackgroundTask>,
    },
    ClaudeAsyncTaskControlChanged {
        task_id: String,
        can_stop: bool,
    },
    /// Claude Code reported the end of one model cycle. It travels on the
    /// same ordered stream as `SessionUpdate`, so everything the adapter sent
    /// before the result is recorded before it.
    ClaudeTurnResult(ClaudeTurnResult),
    ElicitationRequested {
        request: ElicitationRequest,
    },
    ElicitationResolved {
        elicitation_id: String,
        action: String,
    },
    PromptFinished {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<crate::diagnostic::TurnDiagnostic>,

        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<crate::usage::TokenUsage>,
    },
    Notice {
        message: String,
    },
    Warning {
        message: String,
    },
    /// A client terminal started successfully. The worker records an interim
    /// tool call so agents that omit the ACP association do not strand its
    /// eventual result as a standalone transcript item.
    TerminalStarted {
        terminal_id: String,
        command: String,
        started_at_ms: i64,
    },
    /// A client-run terminal was reaped. Exactly one of these is emitted per
    /// terminal, by the supervisor that waits on the child, so kill and
    /// release flow through the same report.
    TerminalClosed {
        terminal_id: String,
        output: String,
        #[serde(default)]
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    UserShellOutput {
        request_id: String,
        command: String,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    UserShellFinished {
        request_id: String,
        result: crate::relay::UserShellResult,
    },
    ConfigApplied {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        key: String,
        value: String,
        /// The complete configuration returned by ACP for this change. Keep
        /// it in the same runtime event as command completion so the relay
        /// cannot publish a checkpoint between the two durable observations.
        #[serde(default)]
        config_options: Vec<SessionConfigOption>,
    },
    GoalControlApplied {
        request_id: String,
    },
    SessionModeApplied {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        mode_id: String,
        #[serde(default)]
        config_options: Vec<SessionConfigOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modes: Option<SessionModeState>,
    },
    CommandRejected {
        request_id: String,
        message: String,
    },
    CommandInterrupted {
        request_id: String,
        message: String,
    },
    CancelApplied {
        request_id: String,
    },
    SteeringUnconfirmed {
        request_id: String,
        message: String,
    },
    SteerApplied {
        request_id: String,
        queued_command_id: String,
    },
    /// The bridge had no running turn for the steer and returned its prompt.
    SteerReturned {
        request_id: String,
        queued_command_id: String,
    },
    CloseApplied {
        request_id: String,
    },
    /// The ACP child died or the protocol broke after a session was open.
    /// The coordinator interrupts in-flight commands; the runtime reloads the
    /// native session on a new bridge instead of stopping the worker.
    HarnessRestarting {
        message: String,
    },
    Stopped,
}

/// One selectable value of a session configuration option, flattened out of
/// the harness's ACP select shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// Every value the harness currently advertises for `key`, in advertised
/// order and with option groups flattened.
///
/// Empty when the harness advertises no such option or exposes it as
/// something other than a select, which callers read as "not configurable".
#[must_use]
/// What one live session's ACP surface offers, for callers outside the chat.
///
/// The phone server and the terminal must agree on which harness drives plan
/// mode through a session mode and which drives it through a configuration
/// key, on whether fast mode is available, and on which values a setting
/// accepts. Those are facts about the harness rather than about the client, so
/// they are answered here once instead of being decided again in each surface.
pub struct AcpSessionFacts(crate::acp::surface::AcpSessionSurface);

impl AcceptedSessionConfig {
    pub fn from_configuration(
        values: &BTreeMap<String, String>,
        options: &[SessionConfigOption],
    ) -> Self {
        let accepted = |key: &str| {
            let option = find_session_config_option(options, key);
            let recorded = values
                .get(key)
                .or_else(|| option.and_then(|option| values.get(&option.id.to_string())))?;
            Some(recorded.clone())
        };
        Self {
            model: accepted("model"),
            effort: accepted("effort"),
        }
    }

    pub fn remember(&mut self, key: &str, value: &str, options: &[SessionConfigOption]) -> bool {
        let is_selector = |canonical: &str| {
            key == canonical
                || find_session_config_option(options, canonical)
                    .is_some_and(|option| option.id.to_string() == key)
        };
        let current = |canonical| {
            let option = find_session_config_option(options, canonical)?;
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            Some(select.current_value.to_string())
        };
        if is_selector("model") {
            self.model = Some(value.to_owned());
            // A model change can reset effort or remove that selector.
            self.effort = current("effort");
        } else if is_selector("effort") {
            self.effort = Some(value.to_owned());
            if let Some(model) = current("model") {
                self.model = Some(model);
            }
        } else {
            return false;
        }
        true
    }

    /// Fold a completed selector command into the durable configuration using
    /// the same accepted pair as the live bridge. Startup advertisements alone
    /// must never replace it with the provider's defaults.
    pub fn record_completed(
        values: &mut BTreeMap<String, String>,
        key: &str,
        value: &str,
        options: &[SessionConfigOption],
    ) {
        let mut accepted = Self::from_configuration(values, options);
        if !accepted.remember(key, value, options) {
            return;
        }
        for canonical in ["model", "effort"] {
            values.remove(canonical);
            if let Some(option) = find_session_config_option(options, canonical) {
                values.remove(&option.id.to_string());
            }
        }
        if let Some(model) = accepted.model {
            values.insert("model".into(), model);
        }
        if let Some(effort) = accepted.effort {
            values.insert("effort".into(), effort);
        }
    }
}

impl AcpSessionFacts {
    /// Read the facts out of one relay operational snapshot.
    pub fn from_operational(
        harness_kind: HarnessKind,
        configuration: &std::collections::BTreeMap<String, String>,
        config_options: &[SessionConfigOption],
        modes: Option<&agent_client_protocol::schema::v1::SessionModeState>,
    ) -> Self {
        let values = configuration
            .iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect();
        let mut surface = crate::acp::surface::AcpSessionSurface::from_configuration(&values);
        surface.set_harness_kind(harness_kind);
        surface.set_config_options(config_options);
        surface.set_session_modes(modes.cloned());
        Self(surface)
    }

    pub fn supports_plan_mode(&self) -> bool {
        self.0.supports_plan_mode()
    }

    pub fn plan_mode_active(&self) -> bool {
        self.0.plan_mode_active()
    }

    pub fn supports_fast_mode(&self) -> bool {
        self.0.supports_fast_mode()
    }

    pub fn fast_mode_active(&self) -> bool {
        self.0.fast_mode_active()
    }

    pub fn current_model(&self) -> Option<&str> {
        self.0.current_model()
    }

    pub fn current_effort(&self) -> Option<&str> {
        self.0.current_effort()
    }

    /// The ACP call that turns plan mode on or off, or a sentence saying why
    /// this harness cannot.
    pub fn plan_control(&self, active: bool) -> Result<PlanControl, &'static str> {
        self.0.plan_control(active).map_err(|error| match error {
            crate::acp::surface::PlanControlError::CodexIncompatible => {
                "This Codex ACP version does not expose collaboration_mode with plan/default values."
            }
            crate::acp::surface::PlanControlError::GrokIncompatible => {
                "This Grok Build version does not expose compatible plan/default modes."
            }
            crate::acp::surface::PlanControlError::Incompatible => {
                "This ACP harness does not expose compatible plan/default modes."
            }
        })
    }
}
use crate::config::HarnessKind;
pub fn plan_review_answer(response: ElicitationResponse) -> (String, Option<String>) {
    let ElicitationResponse::Accept { content } = response else {
        return ("keep_planning".into(), None);
    };
    let action = match content.get(PLAN_REVIEW_ACTION) {
        Some(ElicitationValue::String(action)) => action.clone(),
        _ => "keep_planning".into(),
    };
    let feedback = match content.get(PLAN_REVIEW_FEEDBACK) {
        Some(ElicitationValue::String(feedback)) if !feedback.trim().is_empty() => {
            Some(feedback.clone())
        }
        _ => None,
    };
    (action, feedback)
}

#[cfg(test)]
mod missing_thread_tests {
    use super::*;

    #[test]
    fn codex_reports_a_missing_thread_through_the_wrapped_adapter_message() {
        // What codex-acp actually sends back for a thread with no rollout.
        assert!(error_reports_missing_native_session(
            HarnessKind::Codex,
            "0199f0ba",
            r#"resume ACP session 0199f0ba: Internal error: {"details": "no rollout found for thread id 0199f0ba"}"#
        ));
        // Anything else keeps failing the resume loudly.
        assert!(!error_reports_missing_native_session(
            HarnessKind::Codex,
            "0199f0ba",
            "resume ACP session 0199f0ba: Internal error: session store is locked"
        ));
    }

    #[test]
    fn codex_reports_a_thread_it_never_wrote_as_thread_not_found() {
        // What codex-acp sent when a same-build worker reloaded a thread that
        // was created and never prompted (launch finding I2-7).
        let error = r#"load ACP session 01a0cfd5-7c1e-4f00-9d7e-1a2b3c4d5e6f: Internal error: {"details": "thread not found: 01a0cfd5-7c1e-4f00-9d7e-1a2b3c4d5e6f"}"#;
        assert!(error_reports_missing_native_session(
            HarnessKind::Codex,
            "01a0cfd5-7c1e-4f00-9d7e-1a2b3c4d5e6f",
            error
        ));
        // A missing thread other than the one being resumed is not ours.
        assert!(!error_reports_missing_native_session(
            HarnessKind::Codex,
            "0f0f0f0f-0000-4000-8000-000000000000",
            error
        ));
    }

    #[test]
    fn claude_reports_a_missing_session_only_for_the_session_being_resumed() {
        // What claude-agent-acp actually sends back for a session with no
        // transcript on disk.
        let missing = concat!(
            "resume ACP session 7ee4c940-f82c-4c6a-847f-47e21675e585: ",
            "Resource not found: 7ee4c940-f82c-4c6a-847f-47e21675e585: {\n",
            "  \"uri\": \"7ee4c940-f82c-4c6a-847f-47e21675e585\"\n}"
        );
        assert!(error_reports_missing_native_session(
            HarnessKind::Claude,
            "7ee4c940-f82c-4c6a-847f-47e21675e585",
            missing
        ));
        // A resource error about anything else must never replace the session.
        assert!(!error_reports_missing_native_session(
            HarnessKind::Claude,
            "0f0f0f0f-0000-4000-8000-000000000000",
            missing
        ));
        assert!(!error_reports_missing_native_session(
            HarnessKind::Claude,
            "7ee4c940-f82c-4c6a-847f-47e21675e585",
            "resume ACP session 7ee4c940-f82c-4c6a-847f-47e21675e585: connection closed"
        ));
        // Each harness only recognizes its own wording.
        assert!(!error_reports_missing_native_session(
            HarnessKind::Codex,
            "7ee4c940-f82c-4c6a-847f-47e21675e585",
            missing
        ));
    }

    #[test]
    fn harnesses_that_always_materialize_a_session_never_report_one_missing() {
        for harness in [HarnessKind::Kimi, HarnessKind::Grok, HarnessKind::Muse] {
            assert!(!error_reports_missing_native_session(
                harness,
                "native",
                r#"Resource not found: native: {"uri": "native"}"#
            ));
            assert!(!error_reports_missing_native_session(
                harness,
                "native",
                "no rollout found for thread id native"
            ));
        }
    }

    #[test]
    fn mjolnirs_own_session_info_update_is_not_agent_content() {
        let mjolnir_metadata: SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "session_info_update",
            "_meta": {"mjGoalCapability": null},
        }))
        .unwrap();
        assert!(!session_update_has_native_history(&mjolnir_metadata));
        let agent_content: SessionUpdate = serde_json::from_value(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"},
        }))
        .unwrap();
        assert!(session_update_has_native_history(&agent_content));
    }
}

/// What counts as the harness answering a prompt (#970).
#[cfg(test)]
mod agent_output_tests {
    use super::*;

    fn update(value: serde_json::Value) -> SessionUpdate {
        serde_json::from_value(value).expect("session update fixture")
    }

    #[test]
    fn traffic_the_harness_emits_on_its_own_is_not_an_answer() {
        for value in [
            serde_json::json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
            serde_json::json!({"sessionUpdate": "current_mode_update", "currentModeId": "default"}),
            serde_json::json!({"sessionUpdate": "config_option_update", "configOptions": []}),
            serde_json::json!({"sessionUpdate": "session_info_update"}),
            serde_json::json!({"sessionUpdate": "usage_update", "used": 12, "size": 100}),
        ] {
            assert!(
                !session_update_is_agent_output(&update(value.clone())),
                "{value} must not count as the agent answering"
            );
        }
    }

    /// A turn that only ran tools answered the prompt. Text is not the test:
    /// an agent that edits a file and says nothing has still done the work.
    #[test]
    fn tool_calls_and_text_both_count_as_an_answer() {
        for value in [
            serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "Edit README.md",
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
            }),
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "done"},
            }),
            serde_json::json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "thinking"},
            }),
        ] {
            assert!(
                session_update_is_agent_output(&update(value.clone())),
                "{value} must count as the agent answering"
            );
        }
    }

    /// The exact shape issue #970 reported: the bridge streams its compaction
    /// progress into the swallowed prompt's turn, so counting every message
    /// would hide the loss.
    #[test]
    fn compaction_progress_text_is_not_an_answer() {
        for text in [
            "Compacting...",
            "\n\nCompacting completed.",
            "Compacting failed: out of memory.",
        ] {
            let banner = update(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            }));
            assert!(session_update_is_compaction_banner(&banner), "{text}");
            assert!(!session_update_is_agent_output(&banner), "{text}");
        }
        let answer = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "Compacting the loop saves two allocations."},
        }));
        assert!(!session_update_is_compaction_banner(&answer));
        assert!(session_update_is_agent_output(&answer));
    }

    #[test]
    fn only_a_prompt_that_asks_to_compact_is_answered_by_compacting() {
        let text = |value: &str| vec![ContentBlock::Text(TextContent::new(value))];
        assert!(prompt_requests_compaction(&text("/compact")));
        assert!(prompt_requests_compaction(&text(
            "  /compact keep the plan"
        )));
        assert!(!prompt_requests_compaction(&text("compact the loop")));
        assert!(!prompt_requests_compaction(&text("/context")));
        assert!(!prompt_requests_compaction(&[]));
    }
}

#[cfg(test)]
mod context_command_tests {
    use super::*;
    #[test]
    fn maintenance_parser_matches_whole_command_names() {
        assert_eq!(
            context_command_text(" /CLEAR \n"),
            Some((ContextCommand::Clear, ""))
        );
        assert_eq!(
            context_command_text("/compact preserve decisions"),
            Some((ContextCommand::Compact, "preserve decisions"))
        );
        assert_eq!(context_command_text("/compactfoo"), None);
        assert_eq!(context_command_text("please /clear"), None);
    }
}

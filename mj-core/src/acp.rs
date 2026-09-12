//! Normalized ACP data and controls shared by workers and control surfaces.
#[doc(hidden)]
pub mod dialect;
pub mod step_clock;
pub mod surface;
mod terminal_compat;
use crate::elicitation::*;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::*;
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
    )
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
    },
    SessionStarted {
        native_session_id: String,
        resumed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_mode: Option<String>,
    },
    SessionConfigured {
        config_options: Vec<SessionConfigOption>,
    },
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
    ElicitationRequested {
        request: ElicitationRequest,
    },
    ElicitationResolved {
        elicitation_id: String,
        action: String,
    },
    PromptFinished {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<crate::usage::TokenUsage>,
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
    SteerApplied {
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
            crate::acp::surface::PlanControlError::DeepseekUnsupported => {
                "Plan mode is unsupported in DSH."
            }
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

//! Process-local classification evidence using the shared transcript summary.
use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use mj_core::activity::ActivityFacts;
use mj_core::activity::verdict::*;
use mj_core::config::HarnessKind;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct TurnContextState {
    generation: u64,
    user_prompt_tail: String,
    message_id: Option<String>,
    assistant_text_tail: String,
    last_completed_message: String,
    summary: crate::summary::TranscriptSummary,
    background_commands: usize,
    queued_commands: usize,
    background_inventory: Vec<mj_core::relay::BackgroundCommand>,
    native_agent_ids: Vec<String>,
    session_id: String,
}

/// The prompt that became visible to the harness at this durable observation.
/// A queued or unconfirmed steering prompt has not been delivered yet.
pub fn delivered_prompt_command_id(observation: &mj_core::relay::RelayObservation) -> Option<&str> {
    use mj_core::relay::{RelayCommandOutcome, RelayObservation};
    match observation {
        RelayObservation::CommandStarted { command_id, .. } => Some(command_id),
        RelayObservation::CommandCompleted {
            outcome: RelayCommandOutcome::Steered { queued_command_id },
            ..
        } => Some(queued_command_id),
        _ => None,
    }
}

/// The relay and runtime share this small process-local evidence accumulator.
#[derive(Debug, Default, Clone)]
pub struct TurnContext(Arc<Mutex<TurnContextState>>);

impl TurnContext {
    pub fn reset(&self, prompt: &str) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let generation = state.generation.wrapping_add(1);
        *state = TurnContextState {
            generation,
            user_prompt_tail: tail(prompt, USER_PROMPT_BYTES),
            background_commands: state.background_commands,
            queued_commands: state.queued_commands,
            background_inventory: std::mem::take(&mut state.background_inventory),
            native_agent_ids: std::mem::take(&mut state.native_agent_ids),
            session_id: std::mem::take(&mut state.session_id),
            summary: std::mem::take(&mut state.summary),
            ..Default::default()
        };
    }

    /// Record durable transcript observations through the same path during live work and replay.
    pub fn observe_relay(
        &self,
        observation: &mj_core::relay::RelayObservation,
        prompt: Option<&str>,
    ) {
        use mj_core::relay::{RelayCommandOutcome, RelayObservation};
        match observation {
            RelayObservation::CommandStarted { .. }
            | RelayObservation::CommandCompleted {
                outcome: RelayCommandOutcome::Steered { .. },
                ..
            } => {
                if let Some(prompt) = prompt {
                    self.reset(prompt);
                    self.0
                        .lock()
                        .expect("turn context lock poisoned")
                        .summary
                        .push_user(prompt);
                }
            }
            RelayObservation::SessionUpdate { update } => self.observe(update),
            RelayObservation::TerminalOutput {
                terminal_id,
                output,
                truncated,
                exit_code,
                signal,
            } => {
                self.observe_terminal(&mj_core::transcript::TerminalOutputRecord {
                    terminal_id: terminal_id.clone(),
                    output: output.clone(),
                    truncated: *truncated,
                    exit_code: *exit_code,
                    signal: signal.clone(),
                });
            }
            RelayObservation::CommandCompleted {
                outcome: RelayCommandOutcome::ContextCleared { .. },
                ..
            } => self.clear_history(),
            _ => {}
        }
    }

    /// Invalidate a pending verdict at a lifecycle boundary without discarding evidence.
    pub fn invalidate(&self) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.generation = state.generation.wrapping_add(1);
    }

    pub fn generation(&self) -> u64 {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .generation
    }

    pub fn counts(&self) -> (usize, usize) {
        let state = self.0.lock().expect("turn context lock poisoned");
        (state.background_commands, state.queued_commands)
    }

    pub fn set_counts(&self, background_commands: usize, queued_commands: usize) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        if (state.background_commands, state.queued_commands)
            != (background_commands, queued_commands)
        {
            state.generation = state.generation.wrapping_add(1);
            state.background_commands = background_commands;
            state.queued_commands = queued_commands;
        }
    }

    pub fn set_session_id(&self, session_id: &str) {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .session_id = session_id.into();
    }

    pub fn session_id(&self) -> String {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .session_id
            .clone()
    }

    /// Compare identities as well as counts; identical level reports are not activity.
    pub fn set_background_inventory(
        &self,
        mut commands: Vec<mj_core::relay::BackgroundCommand>,
        mut native_agent_ids: Vec<String>,
    ) {
        commands.sort_by(|a, b| a.id.cmp(&b.id));
        native_agent_ids.sort();
        let mut state = self.0.lock().expect("turn context lock poisoned");
        if state.background_inventory != commands || state.native_agent_ids != native_agent_ids {
            state.background_inventory = commands;
            state.native_agent_ids = native_agent_ids;
            state.generation = state.generation.wrapping_add(1);
        }
    }

    pub fn observe(&self, update: &SessionUpdate) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        if matches!(
            update,
            SessionUpdate::AgentMessageChunk(_)
                | SessionUpdate::AgentThoughtChunk(_)
                | SessionUpdate::ToolCall(_)
                | SessionUpdate::ToolCallUpdate(_)
        ) {
            state.generation = state.generation.wrapping_add(1);
        }
        state.summary.observe(update);
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let id = chunk.message_id.as_ref().map(ToString::to_string);
                if id != state.message_id {
                    complete_message(&mut state);
                    state.message_id = id;
                }
                if let ContentBlock::Text(content) = &chunk.content {
                    // Trim each chunk before appending, so a huge chunk never grows retained state.
                    state
                        .assistant_text_tail
                        .push_str(&tail(&content.text, ASSISTANT_TEXT_BYTES));
                    state.assistant_text_tail =
                        tail(&state.assistant_text_tail, ASSISTANT_TEXT_BYTES);
                }
            }
            SessionUpdate::ToolCall(_) => {
                complete_message(&mut state);
            }
            SessionUpdate::ToolCallUpdate(_) | SessionUpdate::AgentThoughtChunk(_) => {
                complete_message(&mut state)
            }
            _ => {}
        }
    }

    pub fn observe_terminal(&self, record: &mj_core::transcript::TerminalOutputRecord) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.summary.observe_terminal(record);
        state.generation = state.generation.wrapping_add(1);
    }

    pub fn mark_earlier_history_omitted(&self) {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .summary
            .mark_earlier_history_omitted();
    }

    pub fn clear_history(&self) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.summary = Default::default();
        state.user_prompt_tail.clear();
        state.assistant_text_tail.clear();
        state.last_completed_message.clear();
        state.message_id = None;
        state.generation = state.generation.wrapping_add(1);
    }

    pub fn evidence(
        &self,
        harness: HarnessKind,
        phase: TurnPhase,
        facts: &ActivityFacts,
        now_ms: i64,
    ) -> TurnEvidence {
        let state = self.0.lock().expect("turn context lock poisoned");
        let summary = state.summary.latest_user_messages();
        let mut evidence = TurnEvidence {
            harness,
            phase,
            silent_for_s: facts
                .last_acp_activity_at_ms
                .map_or(0, |last| now_ms.saturating_sub(last).max(0) as u64 / 1000),
            tools_in_flight: facts
                .tools_in_flight
                .iter()
                .take(IN_FLIGHT_TOOLS)
                .map(|tool| ToolEvidence {
                    title: tail(
                        &state
                            .summary
                            .entries
                            .iter()
                            .find(|e| e.id == tool.tool_call_id)
                            .map(|e| e.text.clone())
                            .unwrap_or_else(|| "unknown tool".into()),
                        TOOL_TITLE_BYTES,
                    ),
                    running_s: now_ms.saturating_sub(tool.started_at_ms).max(0) as u64 / 1000,
                })
                .collect(),
            transcript_summary: summary.render(48 * 1024),
            background_commands: facts.background_commands,
            queued_commands: facts.queued_commands,
            user_prompt_tail: state.user_prompt_tail.clone(),
            assistant_text_tail: if state.assistant_text_tail.is_empty() {
                state.last_completed_message.clone()
            } else {
                state.assistant_text_tail.clone()
            },
        };
        let mut limit = 48 * 1024;
        while serde_json::to_vec(&evidence)
            .expect("serialize evidence")
            .len()
            > 60 * 1024
        {
            limit /= 2;
            evidence.transcript_summary = summary.render(limit);
        }
        evidence
    }
}

fn complete_message(state: &mut TurnContextState) {
    if !state.assistant_text_tail.is_empty() {
        state.last_completed_message = std::mem::take(&mut state.assistant_text_tail);
    }
    state.message_id = None;
}

fn tail(text: &str, maximum_bytes: usize) -> String {
    let start = text.floor_char_boundary(text.len().saturating_sub(maximum_bytes));
    let mut value = text[start..].to_owned();
    mj_core::transcript::truncate_string_start(&mut value, maximum_bytes);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::activity::InFlightToolCall;
    use serde_json::json;
    fn message(id: &str, text: &str) -> SessionUpdate {
        serde_json::from_value(json!({"sessionUpdate":"agent_message_chunk","messageId":id,"content":{"type":"text","text":text}})).unwrap()
    }

    fn tool(title: &str) -> SessionUpdate {
        serde_json::from_value(json!({"sessionUpdate":"tool_call","toolCallId":title,"title":title,"status":"in_progress"})).unwrap()
    }

    #[test]
    fn verdict_uses_latest_delivered_user_without_tool_history_but_keeps_live_facts() {
        use mj_core::relay::RelayObservation;
        let context = TurnContext::default();
        let start = |id: &str| RelayObservation::CommandStarted {
            command_id: id.into(),
            started_at_ms: 0,
        };
        context.observe_relay(&start("old"), Some("OLD REQUEST"));
        context.observe(&message("old", "OLD ANSWER"));
        context.observe(&tool("active-build"));
        context.observe_relay(&start("new"), Some("CURRENT REQUEST"));
        let noisy: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate":"tool_call", "toolCallId":"noisy", "title":"noisy", "status":"completed",
            "rawInput":{"command":"TOOL_BODY".repeat(20_000)}
        })).unwrap();
        context.observe(&noisy);
        context.observe(&message("new", "CURRENT ANSWER"));
        let facts = ActivityFacts {
            background_commands: 2,
            queued_commands: 3,
            tools_in_flight: vec![InFlightToolCall {
                tool_call_id: "active-build".into(),
                title: Some("active-build".into()),
                status: agent_client_protocol::schema::v1::ToolCallStatus::InProgress,
                started_at_ms: 1_000,
            }],
            ..Default::default()
        };
        let evidence = context.evidence(HarnessKind::Codex, TurnPhase::Running, &facts, 5_000);
        assert!(evidence.transcript_summary.contains("CURRENT REQUEST"));
        assert!(evidence.transcript_summary.contains("CURRENT ANSWER"));
        for excluded in [
            "OLD REQUEST",
            "OLD ANSWER",
            "TOOL_BODY",
            "<tool",
            "bytes omitted",
        ] {
            assert!(
                !evidence.transcript_summary.contains(excluded),
                "{excluded}"
            );
        }
        assert_eq!(evidence.user_prompt_tail, "CURRENT REQUEST");
        assert_eq!(evidence.assistant_text_tail, "CURRENT ANSWER");
        assert_eq!(evidence.background_commands, 2);
        assert_eq!(evidence.queued_commands, 3);
        assert_eq!(evidence.tools_in_flight.len(), 1);
        assert!(evidence.tools_in_flight[0].title.contains("active-build"));
        assert_eq!(evidence.tools_in_flight[0].running_s, 4);
        let full = context.0.lock().unwrap().summary.render(256 * 1024);
        assert!(full.contains("OLD REQUEST") && full.contains("TOOL_BODY"));
    }

    #[test]
    fn evidence_caps_utf8_text_tools_and_durations() {
        let context = TurnContext::default();
        context.reset(&"é".repeat(20_000));
        context.observe(&message("first", &"🦀".repeat(20_000)));
        let first = context.evidence(
            HarnessKind::Claude,
            TurnPhase::Running,
            &ActivityFacts::default(),
            0,
        );
        assert_eq!(first.user_prompt_tail.len(), USER_PROMPT_BYTES);
        assert_eq!(first.assistant_text_tail.len(), ASSISTANT_TEXT_BYTES);
        for n in 0..20 {
            context.observe(&tool(&format!("{n}{}", "é".repeat(300))));
        }
        let facts = ActivityFacts {
            last_acp_activity_at_ms: Some(1_000),
            background_commands: 3,
            queued_commands: 2,
            tools_in_flight: (0..100)
                .map(|n| InFlightToolCall {
                    tool_call_id: n.to_string(),
                    title: Some("🦀".repeat(200)),
                    status: agent_client_protocol::schema::v1::ToolCallStatus::InProgress,
                    started_at_ms: 2_000,
                })
                .collect(),
            ..Default::default()
        };
        let evidence = context.evidence(HarnessKind::Claude, TurnPhase::Running, &facts, 95_000);
        assert_eq!(evidence.tools_in_flight.len(), IN_FLIGHT_TOOLS);
        assert!(
            evidence
                .tools_in_flight
                .iter()
                .all(|tool| tool.title.len() <= TOOL_TITLE_BYTES && tool.running_s == 93)
        );
        assert!(evidence.transcript_summary.len() <= 48 * 1024);
        assert_eq!(evidence.assistant_text_tail, first.assistant_text_tail);
        assert_eq!(evidence.silent_for_s, 94);
        let json = serde_json::to_value(evidence).unwrap();
        assert_eq!(json["phase"], "running");
        assert_eq!(json["harness"], "claude");
        assert_eq!(json["background_commands"], 3);
        assert_eq!(json["queued_commands"], 2);
        assert!(serde_json::to_vec(&json).unwrap().len() < 64 * 1024);
    }

    #[test]
    fn message_boundaries_and_prompt_reset_do_not_mix_answers() {
        let context = TurnContext::default();
        context.reset("First prompt");
        context.observe(&message("one", "Old answer"));
        context.observe(&message("two", "New "));
        context.observe(&message("two", "answer?"));
        let evidence = context.evidence(
            HarnessKind::Claude,
            TurnPhase::Replied,
            &ActivityFacts::default(),
            0,
        );
        assert_eq!(evidence.assistant_text_tail, "New answer?");
        let generation = context.generation();
        context.set_counts(1, 2);
        assert_ne!(context.generation(), generation);
        let generation = context.generation();
        context.set_counts(1, 2);
        assert_eq!(context.generation(), generation);
        assert_eq!(context.counts(), (1, 2));
        context.invalidate();
        assert_ne!(context.generation(), generation);
        assert_eq!(context.counts(), (1, 2));
        let generation = context.generation();
        context.reset("Second prompt");
        assert_eq!(context.counts(), (1, 2));
        assert_ne!(context.generation(), generation);
        let evidence = context.evidence(
            HarnessKind::Claude,
            TurnPhase::Running,
            &ActivityFacts::default(),
            0,
        );
        assert_eq!(evidence.user_prompt_tail, "Second prompt");
        assert!(evidence.assistant_text_tail.is_empty());
        assert!(!evidence.transcript_summary.contains("<user>"));
    }
}

//! Process-local classification evidence using the shared transcript summary.
use agent_client_protocol::v1::schema::{ContentBlock, SessionUpdate};
use mj_core::activity::ActivityFacts;
use mj_core::activity::verdict::*;
use mj_core::config::HarnessKind;
use mj_core::guardian::{MAX_PRIOR_GUARDIAN_REVIEWS, PriorGuardianReview, TrustedUserInput};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct TurnContextState {
    generation: u64,
    parent_activity: Option<std::time::Instant>,
    decision_log: Option<mj_core::jev::DecisionLog>,
    authorization: Option<mj_core::assessment::ContextHistory>,
    user_prompt_tail: Option<String>,
    message_id: Option<String>,
    assistant_text_tail: String,
    last_completed_message: String,
    summary: crate::summary::TranscriptSummary,
    background_commands: usize,
    queued_commands: usize,
    background_inventory: Vec<mj_core::relay::BackgroundCommand>,
    native_agent_ids: Vec<String>,
    session_id: String,
    /// Mjolnir guardian reviews in this request. A new request starts with
    /// none; a recovery continues the same request and keeps them.
    guardian_reviews: GuardianReviews,
    trusted_user_inputs: GuardianUserInputs,
    prior_guardian_reviews: Vec<PriorGuardianReview>,
    // These verdicts have settled but their journal events have not arrived.
    pending_prior_guardian_reviews: VecDeque<PriorGuardianReview>,
}

#[derive(Debug, Default)]
struct GuardianUserInputs {
    durable: Vec<TrustedUserInput>,
    pending: Vec<TrustedUserInput>,
    /// Retained journal records keep their live identity for in-flight reviews.
    live_ids: HashMap<String, String>,
    next_pending_id: u64,
}

/// Codex's per-turn guardian denial circuit breaker
/// (`ext/guardian-reviewer/src/circuit_breaker.rs`): it trips once per request
/// on 3 consecutive denials or 10 denials among the last 50 reviews.
#[derive(Debug, Default)]
struct GuardianReviews {
    consecutive_denials: u32,
    recent: VecDeque<bool>,
    tripped: bool,
}

const MAX_CONSECUTIVE_GUARDIAN_DENIALS: u32 = 3;
const MAX_RECENT_GUARDIAN_DENIALS: usize = 10;
const GUARDIAN_DENIAL_WINDOW: usize = 50;

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
        RelayObservation::MailboxEventsDelivered {
            steering: Some(steering),
            ..
        } => Some(&steering.queued_prompt_id),
        _ => None,
    }
}

/// The relay and runtime share this small process-local evidence accumulator.
#[derive(Debug, Default, Clone)]
pub struct TurnContext(Arc<Mutex<TurnContextState>>);

impl TurnContext {
    /// Process-local parent clock; child traffic and inventory updates never mark it.
    pub fn mark_parent_activity(&self) {
        self.mark_parent_activity_at(std::time::Instant::now());
    }

    /// Record the monotonic time at which parent activity was observed.
    pub fn mark_parent_activity_at(&self, at: std::time::Instant) {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .parent_activity = Some(at);
    }

    pub fn parent_activity(&self) -> Option<std::time::Instant> {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .parent_activity
    }

    pub fn set_decision_log(&self, log: mj_core::jev::DecisionLog) {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .decision_log = Some(log);
    }
    pub fn decision_log(&self) -> Option<mj_core::jev::DecisionLog> {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .decision_log
            .clone()
    }
    /// Start a turn. `user_prompt_tail` carries the prompt only when the
    /// person wrote it: a prompt Mjolnir submitted (a mailbox wake, a child
    /// report, a continuation, a parent's message to a child) is not the
    /// person's latest request, and Jev scored those turns better without it
    /// (.agents/docs/jev-still-open-20261106.md).
    pub fn reset(&self, command_id: Option<&str>, prompt: &str) {
        self.reset_request(command_id, Some(prompt));
    }

    /// Returns whether this delivery establishes a request in the context.
    fn reset_request(&self, command_id: Option<&str>, prompt: Option<&str>) -> bool {
        let recovery = command_id.is_some_and(mj_core::continuation::is_restart_continuation);
        let user_prompt_tail = prompt.map(|prompt| {
            if recovery || mj_core::continuation::is_user_authored_message(command_id, prompt) {
                tail(prompt, USER_PROMPT_BYTES)
            } else {
                String::new()
            }
        });
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let generation = state.generation.wrapping_add(1);
        if recovery {
            if state.user_prompt_tail.is_none()
                && let (Some(user_prompt_tail), Some(prompt)) = (user_prompt_tail, prompt)
            {
                state.user_prompt_tail = Some(user_prompt_tail);
                // The carried request precedes the retained replay suffix.
                // Preserve that suffix and its partially observed answer.
                state.summary.push_user(prompt);
                state.summary.entries.rotate_right(1);
            }
            // Recovery keeps the request, but delivery closes the interrupted
            // message just as the transcript does. An unlabelled chunk from
            // the new harness must not inherit the old message ID.
            complete_message(&mut state);
            state.generation = generation;
            state.parent_activity = Some(std::time::Instant::now());
            return false;
        }
        *state = TurnContextState {
            generation,
            parent_activity: Some(std::time::Instant::now()),
            decision_log: state.decision_log.clone(),
            authorization: state.authorization.clone(),
            trusted_user_inputs: std::mem::take(&mut state.trusted_user_inputs),
            prior_guardian_reviews: std::mem::take(&mut state.prior_guardian_reviews),
            pending_prior_guardian_reviews: std::mem::take(
                &mut state.pending_prior_guardian_reviews,
            ),
            user_prompt_tail,
            background_commands: state.background_commands,
            queued_commands: state.queued_commands,
            background_inventory: std::mem::take(&mut state.background_inventory),
            native_agent_ids: std::mem::take(&mut state.native_agent_ids),
            session_id: std::mem::take(&mut state.session_id),
            summary: std::mem::take(&mut state.summary),
            ..Default::default()
        };
        true
    }

    /// A known prompt delivery whose body may no longer be retained.
    pub fn observe_prompt_delivery(&self, command_id: Option<&str>, prompt: Option<&str>) {
        if self.reset_request(command_id, prompt)
            && let Some(prompt) = prompt
        {
            self.0
                .lock()
                .expect("turn context lock poisoned")
                .summary
                .push_user(prompt);
        }
    }

    /// Begin a turn the relay owner has identified as autonomous.
    pub fn observe_autonomous_turn_start(&self) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.assistant_text_tail.clear();
        state.last_completed_message.clear();
        state.message_id = None;
        state.generation = state.generation.wrapping_add(1);
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
            }
            | RelayObservation::MailboxEventsDelivered {
                steering: Some(_), ..
            } => {
                // A steering confirmation proves delivery even after its
                // queue body was collected. Keep the reply boundary without
                // inventing the missing prompt text.
                if prompt.is_some()
                    || !matches!(observation, RelayObservation::CommandStarted { .. })
                {
                    self.observe_prompt_delivery(delivered_prompt_command_id(observation), prompt);
                }
            }
            // ACP completion may precede native execution's actual end.
            // Deliveries, autonomous starts and these durable bridge/native
            // boundaries own the reply stream instead.
            RelayObservation::SessionRestarted
            | RelayObservation::HarnessTurnSettled {
                prompt_in_flight: false,
                ..
            } => {
                complete_message(&mut self.0.lock().expect("turn context lock poisoned"));
            }
            RelayObservation::ElicitationResolved {
                reply: Some(_),
                action,
                ..
            } if action != mj_core::elicitation::OPERATOR_AWAY_ACTION => {
                complete_message(&mut self.0.lock().expect("turn context lock poisoned"));
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
                    output: output.clone().into(),
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

    /// Publish the relay owner's durable user-instruction history to the ACP
    /// classifier. A change invalidates any request built from older context.
    pub fn set_authorization_context(
        &self,
        authorization: Option<mj_core::assessment::ContextHistory>,
    ) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        if state.authorization != authorization {
            state.authorization = authorization;
            state.generation = state.generation.wrapping_add(1);
        }
    }

    /// Restore the latest human instructions without changing replayed reply boundaries.
    pub fn restore_user_prompt_tail(&self, history: &mj_core::assessment::ContextHistory) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.user_prompt_tail = Some(
            history
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .map(|message| tail(&message.text, USER_PROMPT_BYTES))
                .unwrap_or_default(),
        );
        state.generation = state.generation.wrapping_add(1);
    }

    pub fn authorization_context(&self) -> Option<mj_core::assessment::ContextHistory> {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .authorization
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
        state.parent_activity = Some(std::time::Instant::now());
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
                // Like authorization history, an omitted ID continues the
                // current message; explicit IDs establish a new boundary.
                let id = chunk
                    .message_id
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| state.message_id.clone());
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

    /// The retained transcript as the guardian reviewer reads it: recent
    /// context first, within `limit` bytes.
    pub fn render_transcript(&self, limit: usize) -> String {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .summary
            .render(limit)
    }

    pub fn set_trusted_user_inputs(&self, durable: Vec<TrustedUserInput>) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let history = &mut state.trusted_user_inputs;
        history.durable = durable;
        history.pending.clear();
        history.live_ids.clear();
    }

    /// Record the answer and enqueue its echo in the same order. The
    /// callback must only publish into a channel slot already reserved.
    pub fn record_pending_trusted_user_input(
        &self,
        elicitation_id: &str,
        mut input: TrustedUserInput,
        publish: impl FnOnce(),
    ) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let history = &mut state.trusted_user_inputs;
        input.id = format!("pending:{}:{elicitation_id}", history.next_pending_id);
        history.next_pending_id += 1;
        history.pending.push(input);
        let retained = TrustedUserInput::retained_count(history.pending.iter());
        history.pending.drain(..history.pending.len() - retained);
        publish();
    }

    pub fn sync_trusted_user_inputs(&self, durable: Vec<TrustedUserInput>) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let history = &mut state.trusted_user_inputs;
        history
            .live_ids
            .retain(|id, _| durable.iter().any(|input| &input.id == id));
        // The journal retains a suffix. Match only newly appended entries to
        // pending inputs: an older answer may have reused the same form ID.
        let first_new = durable
            .iter()
            .rposition(|input| history.durable.iter().any(|old| old.id == input.id))
            .map_or(0, |index| index + 1);
        for input in &durable[first_new..] {
            if let Some(elicitation_id) = input.elicitation_id()
                && let Some(index) = history
                    .pending
                    .iter()
                    .position(|pending| pending.elicitation_id() == Some(elicitation_id))
            {
                let pending = history.pending.remove(index);
                history.live_ids.insert(input.id.clone(), pending.id);
            }
        }
        history.durable = durable;
    }

    pub fn trusted_user_inputs(&self) -> Vec<TrustedUserInput> {
        let state = self.0.lock().expect("turn context lock poisoned");
        let history = &state.trusted_user_inputs;
        let inputs = history.durable.iter().chain(&history.pending);
        let retained = TrustedUserInput::retained_count(inputs.clone());
        inputs
            .skip(history.durable.len() + history.pending.len() - retained)
            .map(|input| {
                let mut input = input.clone();
                if let Some(id) = history.live_ids.get(&input.id) {
                    input.id.clone_from(id);
                }
                input
            })
            .collect()
    }

    pub fn record_prior_guardian_review(&self, review: PriorGuardianReview) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let reviews = &mut state.pending_prior_guardian_reviews;
        if reviews.len() == MAX_PRIOR_GUARDIAN_REVIEWS {
            reviews.pop_front();
        }
        reviews.push_back(review);
    }

    pub fn prior_guardian_reviews(&self) -> Vec<PriorGuardianReview> {
        let state = self.0.lock().expect("turn context lock poisoned");
        let reviews = state
            .prior_guardian_reviews
            .iter()
            .chain(&state.pending_prior_guardian_reviews);
        let excess = (state.prior_guardian_reviews.len()
            + state.pending_prior_guardian_reviews.len())
        .saturating_sub(MAX_PRIOR_GUARDIAN_REVIEWS);
        reviews.skip(excess).cloned().collect()
    }

    pub fn set_prior_guardian_reviews(&self, durable: Vec<PriorGuardianReview>) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.prior_guardian_reviews = durable;
        state.pending_prior_guardian_reviews.clear();
    }

    pub fn sync_prior_guardian_reviews(&self, durable: Vec<PriorGuardianReview>) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        // Keep only unsettled journal deliveries. Merging the old durable
        // history too would resurrect entries evicted by the snapshot's bound.
        state.pending_prior_guardian_reviews.retain(|pending| {
            !pending.review_id.as_ref().is_some_and(|id| {
                durable
                    .iter()
                    .any(|review| review.review_id.as_ref() == Some(id))
            })
        });
        state.prior_guardian_reviews = durable;
    }

    pub fn clear_prior_guardian_reviews(&self) {
        self.set_prior_guardian_reviews(Vec::new());
    }

    /// Whether the guardian's denials tripped the breaker in this request, so
    /// its remaining approvals go to the person.
    pub fn guardian_tripped(&self) -> bool {
        self.0
            .lock()
            .expect("turn context lock poisoned")
            .guardian_reviews
            .tripped
    }

    /// Record one completed guardian review in this request. Returns whether
    /// this denial tripped the breaker.
    pub fn record_guardian_review(&self, denied: bool) -> bool {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        let reviews = &mut state.guardian_reviews;
        reviews.consecutive_denials = if denied {
            reviews.consecutive_denials.saturating_add(1)
        } else {
            0
        };
        reviews.recent.push_back(denied);
        if reviews.recent.len() > GUARDIAN_DENIAL_WINDOW {
            reviews.recent.pop_front();
        }
        let recent_denials = reviews.recent.iter().filter(|denied| **denied).count();
        let trips = denied
            && !reviews.tripped
            && (reviews.consecutive_denials >= MAX_CONSECUTIVE_GUARDIAN_DENIALS
                || recent_denials >= MAX_RECENT_GUARDIAN_DENIALS);
        reviews.tripped |= trips;
        trips
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
        state.user_prompt_tail = None;
        state.assistant_text_tail.clear();
        state.last_completed_message.clear();
        state.message_id = None;
        state.authorization = None;
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
        let last_assistant = state
            .summary
            .entries
            .iter()
            .rposition(|e| e.role == crate::summary::SummaryRole::Assistant);
        let final_tool_calls = state
            .summary
            .entries
            .iter()
            .skip(last_assistant.map_or(0, |i| i + 1))
            .filter(|e| e.role == crate::summary::SummaryRole::Tool)
            .take(IN_FLIGHT_TOOLS)
            .map(|e| mj_core::activity::verdict::ToolOutcome {
                name: tool_title(
                    e.tool
                        .as_ref()
                        .and_then(|t| t.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(&e.text),
                ),
                status: e
                    .tool
                    .as_ref()
                    .and_then(|t| t.get("status"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
            })
            .collect();
        TurnEvidence {
            authorization: None,
            final_tool_calls,
            background: Vec::new(),
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
                    title: tool_title(
                        state
                            .summary
                            .entries
                            .iter()
                            .find(|e| e.id == tool.tool_call_id)
                            .map_or("unknown tool", |e| e.text.as_str()),
                    ),
                    running_s: now_ms.saturating_sub(tool.started_at_ms).max(0) as u64 / 1000,
                })
                .collect(),
            transcript_summary: summary.render(48 * 1024),
            background_commands: facts.background_commands,
            queued_commands: facts.queued_commands,
            user_prompt_tail: state.user_prompt_tail.clone().unwrap_or_default(),
            assistant_text_tail: if state.assistant_text_tail.is_empty() {
                state.last_completed_message.clone()
            } else {
                state.assistant_text_tail.clone()
            },
            completion: None,
        }
    }
}

fn complete_message(state: &mut TurnContextState) {
    if !state.assistant_text_tail.is_empty() {
        state.last_completed_message = std::mem::take(&mut state.assistant_text_tail);
    }
    state.message_id = None;
}

/// The last `maximum_bytes` of `text`, cut at a character boundary: the tail
/// every turn classifier receives.
pub fn tail(text: &str, maximum_bytes: usize) -> String {
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

    #[test]
    fn guardian_alignment_keeps_live_answer_identity_when_reused_form_ids_are_journaled() {
        let context = TurnContext::default();
        let elicitation = "form:with:colons";
        let input = |ordinal| {
            TrustedUserInput::from_elicitation(
                format!("elicitation:{ordinal}:{elicitation}"),
                elicitation,
                None,
                "accept",
                None,
                Some("Same reply"),
                None,
            )
            .unwrap()
        };
        let older = input(1);
        context.set_trusted_user_inputs(vec![older.clone()]);

        context.record_pending_trusted_user_input(elicitation, input(2), || {});
        let first_live_view = context.trusted_user_inputs();
        assert_eq!(first_live_view.len(), 2);
        assert_ne!(first_live_view[0].id, first_live_view[1].id);

        context.sync_trusted_user_inputs(vec![older.clone(), input(2)]);
        assert_eq!(context.trusted_user_inputs(), first_live_view);

        context.record_pending_trusted_user_input(elicitation, input(3), || {});
        let second_live_view = context.trusted_user_inputs();
        assert_eq!(second_live_view.len(), 3);
        assert_ne!(second_live_view[1].id, second_live_view[2].id);
        context.sync_trusted_user_inputs(vec![older, input(2), input(3)]);
        assert_eq!(context.trusted_user_inputs(), second_live_view);
    }

    #[test]
    fn guardian_alignment_keeps_bounded_review_history_across_turns() {
        let context = TurnContext::default();
        assert!(!context.record_guardian_review(true));
        assert!(!context.record_guardian_review(true));
        assert!(context.record_guardian_review(true));
        for index in 0..=GUARDIAN_DENIAL_WINDOW {
            context.record_prior_guardian_review(PriorGuardianReview {
                review_id: None,
                tool: "shell".into(),
                action_summary: index.to_string(),
                outcome: mj_core::guardian::Outcome::Deny,
                risk_level: None,
                user_authorization: None,
                rationale: None,
            });
        }
        context.reset(Some("next"), "Next turn");
        assert!(!context.guardian_tripped());
        let reviews = context.prior_guardian_reviews();
        assert_eq!(reviews.len(), GUARDIAN_DENIAL_WINDOW);
        assert_eq!(reviews[0].action_summary, "1");
        assert_eq!(
            reviews.last().unwrap().action_summary,
            GUARDIAN_DENIAL_WINDOW.to_string()
        );
        context.clear_prior_guardian_reviews();
        assert!(context.prior_guardian_reviews().is_empty());
    }

    fn message(id: &str, text: &str) -> SessionUpdate {
        serde_json::from_value(json!({"sessionUpdate":"agent_message_chunk","messageId":id,"content":{"type":"text","text":text}})).unwrap()
    }

    fn tool(title: &str) -> SessionUpdate {
        serde_json::from_value(json!({"sessionUpdate":"tool_call","toolCallId":title,"title":title,"status":"in_progress"})).unwrap()
    }

    #[test]
    fn diagnostic_logging_does_not_change_evidence_or_generation_and_survives_reset() {
        let context = TurnContext::default();
        context.reset(None, "Implement the parser");
        let generation = context.generation();
        let before = serde_json::to_value(context.evidence(
            HarnessKind::Codex,
            TurnPhase::Running,
            &Default::default(),
            0,
        ))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let log = mj_core::jev::DecisionLog::open(dir.path().into()).unwrap();
        context.set_decision_log(log.clone());
        let attempt = log.start("s", "activity", "Who acts?", "Current request");
        attempt.finish("unchanged", "Kept runtime facts");
        assert_eq!(context.generation(), generation);
        assert_eq!(
            serde_json::to_value(context.evidence(
                HarnessKind::Codex,
                TurnPhase::Running,
                &Default::default(),
                0
            ))
            .unwrap(),
            before
        );
        context.reset(None, "New work");
        assert!(context.decision_log().is_some());
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
                status: agent_client_protocol::v1::schema::ToolCallStatus::InProgress,
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
    fn tool_calls_after_the_last_assistant_text_are_named_in_the_evidence() {
        use mj_core::relay::RelayObservation;
        let context = TurnContext::default();
        context.observe_relay(
            &RelayObservation::CommandStarted {
                command_id: "audit".into(),
                started_at_ms: 0,
            },
            Some("Audit the cache and hand back."),
        );
        context.observe(&message("m1", "I'm checking the update paths now."));
        let handback: SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate":"tool_call", "toolCallId":"hb", "title":"mcp__mj-agents__handback",
            "status":"completed", "rawInput":{"report":"done"}
        }))
        .unwrap();
        context.observe(&handback);
        let facts = ActivityFacts::default();
        let evidence = context.evidence(HarnessKind::Codex, TurnPhase::Replied, &facts, 5_000);
        assert_eq!(evidence.final_tool_calls.len(), 1);
        assert!(evidence.final_tool_calls[0].name.contains("handback"));
        assert_eq!(evidence.final_tool_calls[0].status, "completed");
        // Text after the call closes the list again.
        context.observe(&message("m2", "Handed back."));
        let evidence = context.evidence(HarnessKind::Codex, TurnPhase::Replied, &facts, 5_000);
        assert!(evidence.final_tool_calls.is_empty());
    }

    #[test]
    fn evidence_caps_utf8_text_tools_and_durations() {
        let context = TurnContext::default();
        context.reset(None, &"é".repeat(20_000));
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
                    status: agent_client_protocol::v1::schema::ToolCallStatus::InProgress,
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
        context.reset(None, "First prompt");
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
        context.reset(None, "Second prompt");
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
        // A prompt Mjolnir submitted is not the person's latest request.
        context.reset(Some("mailbox-wake-1"), "A child finished its task.");
        let evidence = context.evidence(
            HarnessKind::Claude,
            TurnPhase::Running,
            &ActivityFacts::default(),
            0,
        );
        assert!(evidence.user_prompt_tail.is_empty());
    }
    // Hard-won: 753b1f960: recovery lost its request when the original start left the hot journal.
    #[test]
    fn recovery_initializes_missing_request_and_preserves_restored_corrections() {
        use mj_core::relay::RelayObservation;
        for restored in [false, true] {
            let context = TurnContext::default();
            if restored {
                context.observe_relay(
                    &RelayObservation::CommandStarted {
                        command_id: "correction".into(),
                        started_at_ms: 1,
                    },
                    Some("Use the corrected approach"),
                );
                context.observe(&message("partial", "Partial work"));
            } else {
                context.mark_earlier_history_omitted();
                context.observe(&message("suffix", "Only the tail survived"));
            }
            let recovery = mj_core::continuation::restart_continuation_id("original");
            context.observe_relay(
                &RelayObservation::CommandStarted {
                    command_id: recovery.clone(),
                    started_at_ms: 2,
                },
                Some("Original request"),
            );
            let evidence = context.evidence(
                HarnessKind::Codex,
                TurnPhase::Running,
                &ActivityFacts::default(),
                0,
            );
            assert_eq!(
                evidence.user_prompt_tail,
                if restored {
                    "Use the corrected approach"
                } else {
                    "Original request"
                }
            );
            assert!(evidence.transcript_summary.contains(if restored {
                "Use the corrected approach"
            } else {
                "Original request"
            }));
            assert_eq!(
                evidence.assistant_text_tail,
                if restored {
                    "Partial work"
                } else {
                    "Only the tail survived"
                }
            );
            assert!(evidence.transcript_summary.contains(if restored {
                "Partial work"
            } else {
                "Only the tail survived"
            }));
            let before = evidence.transcript_summary;
            context.observe_relay(
                &RelayObservation::CommandStarted {
                    command_id: mj_core::continuation::restart_continuation_id(&recovery),
                    started_at_ms: 3,
                },
                Some("Original request"),
            );
            assert_eq!(
                context
                    .evidence(
                        HarnessKind::Codex,
                        TurnPhase::Running,
                        &ActivityFacts::default(),
                        0
                    )
                    .transcript_summary,
                before
            );
        }
    }
}

//! Bounded turn evidence and conservative decisions for the optional Jev classifier.
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;

use super::ActivityFacts;
use crate::config::HarnessKind;
use crate::transcript::truncate_string_start;

pub const USER_PROMPT_BYTES: usize = 1024;
pub const ASSISTANT_TEXT_BYTES: usize = 2048;
pub const TOOL_TITLE_BYTES: usize = 128;
pub const RECENT_TOOLS: usize = 8;
pub const IN_FLIGHT_TOOLS: usize = 16;
/// Jev recommends high confidence for automation; weaker answers preserve current behavior.
pub const ACT_CONFIDENCE: f32 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    Replied,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolEvidence {
    pub title: String,
    pub running_s: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnEvidence {
    pub harness: HarnessKind,
    pub phase: TurnPhase,
    pub silent_for_s: u64,
    pub tools_in_flight: Vec<ToolEvidence>,
    pub recent_tools: Vec<String>,
    pub background_commands: usize,
    pub queued_commands: usize,
    pub user_prompt_tail: String,
    pub assistant_text_tail: String,
}

#[derive(Debug, Default)]
struct TurnContextState {
    generation: u64,
    user_prompt_tail: String,
    message_id: Option<String>,
    assistant_text_tail: String,
    last_completed_message: String,
    recent_tools: VecDeque<String>,
    background_commands: usize,
    queued_commands: usize,
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
            ..Default::default()
        };
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

    pub fn observe(&self, update: &SessionUpdate) {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        state.generation = state.generation.wrapping_add(1);
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
                    truncate_string_start(&mut state.assistant_text_tail, ASSISTANT_TEXT_BYTES);
                }
            }
            SessionUpdate::ToolCall(call) => {
                complete_message(&mut state);
                if state.recent_tools.len() == RECENT_TOOLS {
                    state.recent_tools.pop_front();
                }
                state
                    .recent_tools
                    .push_back(tail(&call.title, TOOL_TITLE_BYTES));
            }
            SessionUpdate::ToolCallUpdate(_) | SessionUpdate::AgentThoughtChunk(_) => {
                complete_message(&mut state)
            }
            _ => {}
        }
    }

    pub fn evidence(
        &self,
        harness: HarnessKind,
        phase: TurnPhase,
        facts: &ActivityFacts,
        now_ms: i64,
    ) -> TurnEvidence {
        let state = self.0.lock().expect("turn context lock poisoned");
        TurnEvidence {
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
                        tool.title.as_deref().unwrap_or("unknown tool"),
                        TOOL_TITLE_BYTES,
                    ),
                    running_s: now_ms.saturating_sub(tool.started_at_ms).max(0) as u64 / 1000,
                })
                .collect(),
            recent_tools: state.recent_tools.iter().cloned().collect(),
            background_commands: facts.background_commands,
            queued_commands: facts.queued_commands,
            user_prompt_tail: state.user_prompt_tail.clone(),
            assistant_text_tail: if state.assistant_text_tail.is_empty() {
                state.last_completed_message.clone()
            } else {
                state.assistant_text_tail.clone()
            },
        }
    }
}

fn complete_message(state: &mut TurnContextState) {
    if !state.assistant_text_tail.is_empty() {
        state.last_completed_message = std::mem::take(&mut state.assistant_text_tail);
    }
    state.message_id = None;
}

fn tail(text: &str, maximum_bytes: usize) -> String {
    // Reuse the transcript's UTF-8-safe tail rule on an already bounded slice.
    let mut start = text.len().saturating_sub(maximum_bytes);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut value = text[start..].to_owned();
    truncate_string_start(&mut value, maximum_bytes);
    value
}

pub fn questions() -> Value {
    serde_json::from_str(include_str!("verdict_questions.json"))
        .expect("bundled turn verdict questions are valid JSON")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingOn {
    User,
    BackgroundWork,
    StillWorking,
    Finished,
    Unclear,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnVerdict {
    pub waiting_on: WaitingOn,
    pub confidence: f32,
    pub asked_question: f32,
}

impl TurnVerdict {
    /// Parse the HTTP response shape documented by TypeSafe, including Noul's object wrapper.
    pub fn parse(response: &Value) -> Result<Self> {
        let answers = response.get("answers").context("missing verdict answers")?;
        let choice = &answers["waiting_on"];
        let waiting_on = match choice["choice"]
            .as_str()
            .context("missing waiting_on choice")?
        {
            "user" => WaitingOn::User,
            "background_work" => WaitingOn::BackgroundWork,
            "still_working" => WaitingOn::StillWorking,
            "finished" => WaitingOn::Finished,
            _ => WaitingOn::Unclear,
        };
        Ok(Self {
            waiting_on,
            confidence: probability(&choice["confidence"])?,
            asked_question: probability(&answers["asked_question"]["noul"])?,
        })
    }
}

fn probability(value: &Value) -> Result<f32> {
    let number = value.as_f64().context("missing verdict probability")?;
    ensure!(
        number.is_finite() && (0.0..=1.0).contains(&number),
        "invalid verdict probability"
    );
    Ok(number as f32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    AwaitingInput,
    ExpectContinuation,
    KeepCurrent,
}

pub fn decide(phase: TurnPhase, verdict: &TurnVerdict) -> Decision {
    if !verdict.confidence.is_finite() || !(ACT_CONFIDENCE..=1.0).contains(&verdict.confidence) {
        return Decision::KeepCurrent;
    }
    match (phase, verdict.waiting_on) {
        (TurnPhase::Running, WaitingOn::User) => Decision::AwaitingInput,
        (TurnPhase::Replied, WaitingOn::BackgroundWork) => Decision::ExpectContinuation,
        _ => Decision::KeepCurrent,
    }
}

/// Resolve once during startup, outside an event or render loop. Never log this value.
pub fn api_key() -> Option<String> {
    resolve_key(
        std::env::var("TYPESAFE_API_KEY").ok().as_deref(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

fn resolve_key(environment: Option<&str>, home: Option<&Path>) -> Option<String> {
    fn nonblank(value: &str) -> Option<String> {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }
    environment.and_then(nonblank).or_else(|| {
        let value =
            std::fs::read_to_string(home?.join(".secrets").join("typesafe_api_key")).ok()?;
        nonblank(&value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::InFlightToolCall;
    use serde_json::json;

    fn message(id: &str, text: &str) -> SessionUpdate {
        serde_json::from_value(json!({"sessionUpdate":"agent_message_chunk","messageId":id,"content":{"type":"text","text":text}})).unwrap()
    }

    fn tool(title: &str) -> SessionUpdate {
        serde_json::from_value(json!({"sessionUpdate":"tool_call","toolCallId":title,"title":title,"status":"in_progress"})).unwrap()
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
        assert_eq!(evidence.recent_tools.len(), RECENT_TOOLS);
        assert!(
            evidence
                .recent_tools
                .iter()
                .all(|title| title.len() <= TOOL_TITLE_BYTES)
        );
        assert_eq!(evidence.assistant_text_tail, first.assistant_text_tail);
        assert_eq!(evidence.silent_for_s, 94);
        let json = serde_json::to_value(evidence).unwrap();
        assert_eq!(json["phase"], "running");
        assert_eq!(json["harness"], "claude");
        assert_eq!(json["background_commands"], 3);
        assert_eq!(json["queued_commands"], 2);
        assert!(serde_json::to_vec(&json).unwrap().len() < 10_000);
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
        assert!(evidence.recent_tools.is_empty());
    }

    #[test]
    fn decisions_require_the_right_phase_choice_and_confidence() {
        for phase in [TurnPhase::Running, TurnPhase::Replied] {
            for waiting_on in [
                WaitingOn::User,
                WaitingOn::BackgroundWork,
                WaitingOn::StillWorking,
                WaitingOn::Finished,
                WaitingOn::Unclear,
            ] {
                for confidence in [0.0, 0.849, 0.85, 0.99, 1.0, 1.01, f32::NAN] {
                    let verdict = TurnVerdict {
                        waiting_on,
                        confidence,
                        asked_question: 0.0,
                    };
                    let expected = match (phase, waiting_on) {
                        (TurnPhase::Running, WaitingOn::User)
                            if (ACT_CONFIDENCE..=1.0).contains(&confidence) =>
                        {
                            Decision::AwaitingInput
                        }
                        (TurnPhase::Replied, WaitingOn::BackgroundWork)
                            if (ACT_CONFIDENCE..=1.0).contains(&confidence) =>
                        {
                            Decision::ExpectContinuation
                        }
                        _ => Decision::KeepCurrent,
                    };
                    assert_eq!(decide(phase, &verdict), expected);
                }
            }
        }
    }

    #[test]
    fn documented_response_parses_and_invalid_probabilities_fail_closed() {
        let mut response = json!({"answers":{"waiting_on":{"type":"choice","choice":"user","confidence":0.95,"probabilities":{"user":0.99}},"asked_question":{"type":"noul","noul":0.97}}});
        let verdict = TurnVerdict::parse(&response).unwrap();
        assert_eq!(verdict.waiting_on, WaitingOn::User);
        assert_eq!(verdict.asked_question, 0.97);
        response["answers"]["waiting_on"]["choice"] = json!("future_choice");
        assert_eq!(
            TurnVerdict::parse(&response).unwrap().waiting_on,
            WaitingOn::Unclear
        );
        response["answers"]["waiting_on"]["confidence"] = json!(1.01);
        assert!(TurnVerdict::parse(&response).is_err());
        response["answers"]["waiting_on"]["confidence"] = json!(0.95);
        response["answers"]["asked_question"]["noul"] = json!(-0.1);
        assert!(TurnVerdict::parse(&response).is_err());
        assert!(TurnVerdict::parse(&json!({})).is_err());
    }

    #[test]
    fn key_resolution_prefers_nonblank_environment_and_trims_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".secrets")).unwrap();
        let file = home.path().join(".secrets/typesafe_api_key");
        std::fs::write(&file, " file-key\n").unwrap();
        assert_eq!(
            resolve_key(Some(" env-key \n"), Some(home.path())).as_deref(),
            Some("env-key")
        );
        assert_eq!(
            resolve_key(Some("  "), Some(home.path())).as_deref(),
            Some("file-key")
        );
        assert_eq!(
            resolve_key(None, Some(home.path())).as_deref(),
            Some("file-key")
        );
        std::fs::write(&file, " \n").unwrap();
        assert_eq!(resolve_key(None, Some(home.path())), None);
        assert_eq!(resolve_key(None, None), None);
    }
}

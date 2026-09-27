//! One semantic assessment and durable action for a completed physical turn.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::verdict::{CompletionEvidence, TurnEvidence};
use crate::continuation::{ASSISTANT_BYTES, EvidenceMessage, USER_BYTES};

pub const PROTOCOL: u32 = 25;
pub const AUTOMATION_CONFIDENCE: f32 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    None,
    TransientProvider,
    Quota,
    Other,
    Unclear,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    None,
    RedundantRequest,
    Required,
    Unclear,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Work {
    Finished,
    AuthorizedUnfinished,
    Waiting,
    Unclear,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Judgment<T> {
    pub choice: T,
    pub confidence: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub failure: Judgment<Failure>,
    pub input: Judgment<Input>,
    pub work: Judgment<Work>,
}
impl Verdict {
    pub fn parse(response: &Value) -> Result<Self> {
        fn choice<T: serde::de::DeserializeOwned>(
            answers: &Value,
            key: &str,
        ) -> Result<Judgment<T>> {
            let answer = &answers[key];
            ensure!(answer["type"] == "choice", "invalid {key} answer type");
            let confidence = answer["confidence"]
                .as_f64()
                .context("missing confidence")?;
            ensure!(
                confidence.is_finite() && (0.0..=1.0).contains(&confidence),
                "invalid confidence"
            );
            Ok(Judgment {
                choice: serde_json::from_value(answer["choice"].clone())?,
                confidence: confidence as f32,
            })
        }
        let answers = response
            .get("answers")
            .context("missing assessment answers")?;
        Ok(Self {
            failure: choice(answers, "failure")?,
            input: choice(answers, "input")?,
            work: choice(answers, "work")?,
        })
    }
    pub fn action(self, authorization_complete: bool) -> Action {
        if self.input.choice == Input::Required && self.input.confidence >= 0.85 {
            return Action::AwaitInput;
        }

        if self.failure.confidence >= AUTOMATION_CONFIDENCE {
            match self.failure.choice {
                Failure::TransientProvider => return Action::RetryProvider,
                Failure::Quota => return Action::RecoverQuota,
                Failure::Other => return Action::AwaitInput,
                _ => {}
            }
        }
        // Unknown failures must not become ordinary nudges.
        if self.failure.choice != Failure::None || self.failure.confidence < AUTOMATION_CONFIDENCE {
            return Action::Uncertain;
        }
        if authorization_complete
            && self.work.choice == Work::AuthorizedUnfinished
            && self.work.confidence >= AUTOMATION_CONFIDENCE
            && matches!(self.input.choice, Input::None | Input::RedundantRequest)
            && self.input.confidence >= AUTOMATION_CONFIDENCE
        {
            return Action::Continue;
        }
        if self.work.confidence >= 0.85
            && self.input.choice == Input::None
            && self.input.confidence >= 0.85
        {
            return match self.work.choice {
                Work::Finished => Action::Finished,
                Work::Waiting => Action::Wait,
                _ => Action::Uncertain,
            };
        }
        Action::Uncertain
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    RetryProvider,
    RecoverQuota,
    Continue,
    AwaitInput,
    Finished,
    Wait,
    Uncertain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Assessed,
    Deferred,
    Scheduled,
    Consumed,
    Superseded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAssessment {
    pub turn_id: String,
    pub completed_ordinal: u64,
    pub revision: u64,
    pub completed_at_ms: i64,
    pub completion: CompletionEvidence,
    pub evidence: Option<TurnEvidence>,
    pub verdict: Option<Verdict>,
    pub action: Option<Action>,
    pub status: Status,
    pub reason: String,
    pub failures: u32,
    pub retry_at_ms: Option<i64>,
}
impl TurnAssessment {
    pub fn pending(turn_id: String, ordinal: u64, at: i64, completion: CompletionEvidence) -> Self {
        Self {
            turn_id,
            completed_ordinal: ordinal,
            revision: ordinal,
            completed_at_ms: at,
            completion,
            evidence: None,
            verdict: None,
            action: None,
            status: Status::Pending,
            reason: "awaiting_classification".into(),
            failures: 0,
            retry_at_ms: None,
        }
    }
    pub fn current(&self) -> bool {
        !matches!(self.status, Status::Consumed | Status::Superseded)
    }
    pub fn needs_classification(&self, now: i64) -> bool {
        matches!(self.status, Status::Pending | Status::Failed)
            && self.retry_at_ms.is_none_or(|at| at <= now)
    }
}

/// Public status without transcript evidence or provider diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub turn_id: String,
    pub revision: u64,
    pub status: Status,
    pub action: Option<Action>,
    pub reason: String,
    pub retry_at_ms: Option<i64>,
}
impl From<&TurnAssessment> for Summary {
    fn from(a: &TurnAssessment) -> Self {
        Self {
            turn_id: a.turn_id.clone(),
            revision: a.revision,
            status: a.status,
            action: a.action,
            reason: a.reason.clone(),
            retry_at_ms: a.retry_at_ms,
        }
    }
}

/// Whole authorization messages, retained independently of the rendering summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHistory {
    pub messages: Vec<EvidenceMessage>,
    pub authorization_complete: bool,
    pub assistant_history_omitted: bool,
    #[serde(default)]
    pub open_assistant_id: Option<String>,
    #[serde(default)]
    pub final_reply_omitted: bool,
}
impl Default for ContextHistory {
    fn default() -> Self {
        Self {
            messages: vec![],
            authorization_complete: true,
            assistant_history_omitted: false,
            open_assistant_id: None,
            final_reply_omitted: false,
        }
    }
}
impl ContextHistory {
    pub fn user(&mut self, id: &str, prompt: &[agent_client_protocol::schema::v1::ContentBlock]) {
        use agent_client_protocol::schema::v1::ContentBlock;
        self.open_assistant_id = None;
        if crate::continuation::is_generated_prompt(id) {
            return;
        }
        let mut text = String::new();
        for block in prompt {
            if let ContentBlock::Text(t) = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            } else {
                self.authorization_complete = false;
            }
        }
        if crate::continuation::is_generated_prompt_text(&text) {
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        if self
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.text.len())
            .sum::<usize>()
            + text.len()
            > USER_BYTES
            || self.messages.len() >= 256
        {
            self.authorization_complete = false;
            return;
        }
        self.messages.push(EvidenceMessage {
            id: format!("user:{id}"),
            role: "user".into(),
            text,
        });
    }
    pub fn assistant(&mut self, id: Option<&str>, ordinal: u64, text: &str) {
        let id = id
            .map(str::to_owned)
            .or_else(|| self.open_assistant_id.clone())
            .unwrap_or_else(|| format!("agent:{ordinal}"));
        if self.open_assistant_id.as_deref() == Some(&id) {
            if let Some(last) = self
                .messages
                .last_mut()
                .filter(|m| m.role == "assistant" && m.id == id)
            {
                if last.text.len() + text.len() <= ASSISTANT_BYTES {
                    last.text.push_str(text);
                } else {
                    self.messages.pop();
                    self.assistant_history_omitted = true;
                    self.final_reply_omitted = true;
                }
            }
        } else {
            self.final_reply_omitted = false;
            self.messages.push(EvidenceMessage {
                id: id.clone(),
                role: "assistant".into(),
                text: text.into(),
            });
        }
        self.open_assistant_id = Some(id);
        while self
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.text.len())
            .sum::<usize>()
            > ASSISTANT_BYTES
            || self.messages.len() > 256
        {
            if let Some(i) = self.messages.iter().position(|m| m.role == "assistant") {
                if i + 1 == self.messages.len() {
                    self.final_reply_omitted = true;
                }
                self.messages.remove(i);
                self.assistant_history_omitted = true;
            } else {
                self.authorization_complete = false;
                break;
            }
        }
    }
    pub fn evidence(&self) -> crate::continuation::ContinuationEvidence {
        crate::continuation::ContinuationEvidence {
            messages: self.messages.clone(),
            assistant_history_omitted: self.assistant_history_omitted,
            quota_message: None,
        }
    }
}

/// Kept at the archive boundary as JSON so canonical equality remains exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u32,
    pub context: Option<ContextHistory>,
    pub assessment: Option<TurnAssessment>,
    pub continuation: crate::continuation::ContinuationState,
    pub capacity_retry: Option<crate::relay::CapacityRetry>,
    pub turn_completion: Option<crate::activity::verdict::TurnCompletion>,
}
impl Checkpoint {
    pub fn decode(value: &Value) -> Result<Self> {
        let state: Self = serde_json::from_value(value.clone())?;
        ensure!(
            state.version == 1,
            "unsupported assessment checkpoint version"
        );
        ensure!(
            serde_json::to_vec(value)?.len() <= 256 * 1024,
            "assessment checkpoint exceeds budget"
        );
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn verdict(failure: Failure, input: Input, work: Work) -> Verdict {
        Verdict {
            failure: Judgment {
                choice: failure,
                confidence: 0.99,
            },
            input: Judgment {
                choice: input,
                confidence: 0.99,
            },
            work: Judgment {
                choice: work,
                confidence: 0.99,
            },
        }
    }
    #[test]
    fn provider_recovery_is_independent_of_uncertain_remaining_work() {
        let mut v = verdict(Failure::TransientProvider, Input::Unclear, Work::Unclear);
        v.failure.confidence = 0.91;
        v.input.confidence = 0.32;
        v.work.confidence = 0.24;
        assert_eq!(v.action(false), Action::RetryProvider);
        v.failure.confidence = 0.89;
        assert_eq!(v.action(true), Action::Uncertain);
        v.failure = Judgment {
            choice: Failure::Quota,
            confidence: 0.99,
        };
        assert_eq!(v.action(false), Action::RecoverQuota);
    }
    #[test]
    fn continuation_requires_complete_authorization_and_no_real_question() {
        for input in [Input::None, Input::RedundantRequest] {
            let v = verdict(Failure::None, input, Work::AuthorizedUnfinished);
            assert_eq!(v.action(true), Action::Continue);
            assert_eq!(v.action(false), Action::Uncertain);
        }
        assert_eq!(
            verdict(Failure::None, Input::Required, Work::AuthorizedUnfinished).action(true),
            Action::AwaitInput
        );
        assert_eq!(
            verdict(Failure::Other, Input::None, Work::AuthorizedUnfinished).action(true),
            Action::AwaitInput
        );
    }
    #[test]
    fn explicit_user_handoff_blocks_provider_automation() {
        assert_eq!(
            verdict(Failure::TransientProvider, Input::Required, Work::Unclear).action(false),
            Action::AwaitInput
        );
        assert_eq!(
            verdict(Failure::Quota, Input::Required, Work::Unclear).action(true),
            Action::AwaitInput
        );
    }
    #[test]
    fn parser_rejects_unknown_choices_and_invalid_confidence() {
        let mut body = serde_json::json!({"answers":{
            "failure":{"type":"choice","choice":"transient_provider","confidence":0.91},
            "input":{"type":"choice","choice":"unclear","confidence":0.32},
            "work":{"type":"choice","choice":"unclear","confidence":0.24}
        }});
        assert_eq!(
            Verdict::parse(&body).unwrap().action(false),
            Action::RetryProvider
        );
        body["answers"]["failure"]["confidence"] = serde_json::json!(1.1);
        assert!(Verdict::parse(&body).is_err());
        body["answers"]["failure"]["confidence"] = serde_json::json!(0.99);
        body["answers"]["failure"]["choice"] = serde_json::json!("guess");
        assert!(Verdict::parse(&body).is_err());
    }
    #[test]
    fn authorization_survives_assistant_eviction_without_clipping_user_consent() {
        use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
        let mut context = ContextHistory::default();
        let prompt = vec![ContentBlock::Text(TextContent::new(
            "Implement, test, and commit.",
        ))];
        context.user("user-command", &prompt);
        context.user("server-retry-10", &prompt);
        for i in 0..40 {
            context.assistant(Some(&format!("message-{i}")), i, &"x".repeat(1024));
        }
        assert!(context.authorization_complete);
        assert!(context.assistant_history_omitted);
        assert_eq!(
            context.messages.iter().filter(|m| m.role == "user").count(),
            1
        );
        context.user(
            "too-large",
            &[ContentBlock::Text(TextContent::new(
                "x".repeat(USER_BYTES + 1),
            ))],
        );
        assert!(!context.authorization_complete);
        assert!(context.messages.iter().all(|m| m.id != "user:too-large"));
    }
}

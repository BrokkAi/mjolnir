//! One semantic assessment and durable action for a completed physical turn.
use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::verdict::{CompletionEvidence, TurnEvidence};
use crate::continuation::{ASSISTANT_BYTES, EvidenceMessage, USER_BYTES};

pub const PROTOCOL: u32 = 25;
pub const AUTOMATION_CONFIDENCE: f32 = 0.90;
pub const REQUIRED_INPUT_PROBABILITY: f64 = 0.50;
pub const REQUIRED_INPUT_RATIO: f64 = 2.5;
pub const FINISHED_WORK_PROBABILITY: f64 = 0.80;
// Provider scores are rounded; tolerate floating point representation at a boundary.
const PROBABILITY_EPSILON: f64 = 1e-12;

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

/// Whether anyone still depends on the background commands the session holds.
/// Asked only when leftover processes are the one thing keeping a session
/// from being quiet; see `mj_core::activity::quiet_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    Needed,
    Unneeded,
    Unclear,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judgment<T> {
    pub choice: T,
    pub confidence: f32,
    /// Full provider distribution, separate from confidence. Empty only in old
    /// durable judgments; their previously stored action remains authoritative.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probabilities: BTreeMap<String, f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub failure: Judgment<Failure>,
    pub input: Judgment<Input>,
    pub work: Judgment<Work>,
    /// Absent from answers older proxies return, and ignored unless the
    /// request listed background commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Judgment<Background>>,
}
impl Verdict {
    pub fn parse(response: &Value) -> Result<Self> {
        fn choice<T: serde::de::DeserializeOwned>(
            answers: &Value,
            key: &str,
            allowed: &[&str],
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
            let probabilities: BTreeMap<String, f64> = serde_json::from_value(
                answer
                    .get("probabilities")
                    .context(format!("missing {key} probabilities"))?
                    .clone(),
            )
            .with_context(|| format!("invalid {key} probabilities"))?;
            ensure!(
                probabilities.len() == allowed.len()
                    && allowed.iter().all(|name| probabilities.contains_key(*name)),
                "incomplete {key} probabilities"
            );
            ensure!(
                probabilities
                    .values()
                    .all(|p| p.is_finite() && (0.0..=1.0).contains(p))
                    && (probabilities.values().sum::<f64>() - 1.0).abs()
                        <= 0.005 * allowed.len() as f64 + PROBABILITY_EPSILON,
                "invalid {key} probability distribution"
            );
            let selected = answer["choice"].as_str().context("missing choice")?;
            ensure!(
                probabilities
                    .get(selected)
                    .is_some_and(|winner| probabilities
                        .values()
                        .all(|p| *winner + PROBABILITY_EPSILON >= *p)),
                "{key} choice is not a probability winner"
            );
            Ok(Judgment {
                choice: serde_json::from_value(answer["choice"].clone())?,
                confidence: confidence as f32,
                probabilities,
            })
        }
        let answers = response
            .get("answers")
            .context("missing assessment answers")?;
        Ok(Self {
            failure: choice(
                answers,
                "failure",
                &["none", "transient_provider", "quota", "other", "unclear"],
            )?,
            input: choice(
                answers,
                "input",
                &["none", "redundant_request", "required", "unclear"],
            )?,
            work: choice(
                answers,
                "work",
                &["finished", "authorized_unfinished", "waiting", "unclear"],
            )?,
            background: answers
                .get("background")
                .filter(|answer| !answer.is_null())
                .map(|_| choice(answers, "background", &["needed", "unneeded", "unclear"]))
                .transpose()?,
        })
    }
    pub fn requires_input(&self) -> bool {
        self.input.choice == Input::Required
            && self
                .input
                .probabilities
                .get("required")
                .is_some_and(|winner| {
                    let runner_up = self
                        .input
                        .probabilities
                        .iter()
                        .filter(|(name, _)| name.as_str() != "required")
                        .map(|(_, probability)| *probability)
                        .fold(0.0_f64, f64::max);
                    *winner + PROBABILITY_EPSILON >= REQUIRED_INPUT_PROBABILITY
                        && *winner + PROBABILITY_EPSILON >= REQUIRED_INPUT_RATIO * runner_up
                })
    }

    pub fn action(&self, authorization_complete: bool) -> Action {
        // A confident quota stop always recovers, whatever the input answer:
        // question detection is unreliable, and the resume prompt tells the
        // agent to ask again and stop if a decision is still open.
        if self.failure.choice == Failure::Quota && self.failure.confidence >= AUTOMATION_CONFIDENCE
        {
            return Action::RecoverQuota;
        }
        if self.requires_input() {
            return Action::AwaitInput;
        }

        if self.failure.confidence >= AUTOMATION_CONFIDENCE {
            match self.failure.choice {
                Failure::TransientProvider => return Action::RetryProvider,
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
        if self.input.choice == Input::None && self.input.confidence >= 0.85 {
            return match self.work.choice {
                Work::Finished
                    if self
                        .work
                        .probabilities
                        .get("finished")
                        .is_some_and(|p| *p + PROBABILITY_EPSILON >= FINISHED_WORK_PROBABILITY) =>
                {
                    Action::Finished
                }
                Work::Waiting if self.work.confidence >= 0.85 => Action::Wait,
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
/// Most messages fit whole; a pasted document is kept by its head and tail
/// with an explicit marker, so one paste cannot use up the whole budget.
pub const MESSAGE_HEAD_BYTES: usize = 4 * 1024;
pub const MESSAGE_TAIL_BYTES: usize = 2 * 1024;
/// Total entries the wire contract allows (`ContinuationEvidence::validate`
/// and the proxy validator). Assistant entries give way first; only a
/// history with this many user messages on its own is refused.
pub const MAX_MESSAGES: usize = 256;

/// Keep the first [`MESSAGE_HEAD_BYTES`] and last [`MESSAGE_TAIL_BYTES`] of
/// a long message, with the omitted byte count in between.
#[must_use]
pub fn trim_middle(text: &str) -> String {
    if text.len() <= MESSAGE_HEAD_BYTES + MESSAGE_TAIL_BYTES {
        return text.to_owned();
    }
    let head_end = text.floor_char_boundary(MESSAGE_HEAD_BYTES);
    let tail_start = text.ceil_char_boundary(text.len() - MESSAGE_TAIL_BYTES);
    format!(
        "{}\n[... {} bytes omitted from the middle of this message ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

impl ContextHistory {
    fn user_bytes(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.text.len())
            .sum()
    }

    fn user_count(&self) -> usize {
        self.messages.iter().filter(|m| m.role == "user").count()
    }

    fn assistant_bytes(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.text.len())
            .sum()
    }

    /// Evict the oldest assistant entry that is not the one being written.
    /// Returns false when none remains.
    fn evict_oldest_assistant(&mut self) -> bool {
        let protected = self.open_assistant_id.clone();
        let Some(i) = self
            .messages
            .iter()
            .position(|m| m.role == "assistant" && Some(&m.id) != protected.as_ref())
        else {
            return false;
        };
        if i + 1 == self.messages.len() {
            self.final_reply_omitted = true;
        }
        self.messages.remove(i);
        self.assistant_history_omitted = true;
        true
    }

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
        let text = trim_middle(&text);
        // The byte budget is for user text alone; a message that would push
        // it over is the one case that makes the history incomplete.
        if self.user_bytes() + text.len() > USER_BYTES || self.user_count() >= MAX_MESSAGES {
            self.authorization_complete = false;
            return;
        }
        // Assistant entries make room; the wire cap counts both roles.
        while self.messages.len() >= MAX_MESSAGES && self.evict_oldest_assistant() {}
        if self.messages.len() >= MAX_MESSAGES {
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
                last.text.push_str(text);
                if last.text.len() > MESSAGE_HEAD_BYTES + MESSAGE_TAIL_BYTES {
                    last.text = trim_middle(&last.text);
                }
            }
        } else {
            self.final_reply_omitted = false;
            self.messages.push(EvidenceMessage {
                id: id.clone(),
                role: "assistant".into(),
                text: trim_middle(text),
            });
        }
        self.open_assistant_id = Some(id);
        // Oldest assistant entries give way to the byte budget and the wire
        // cap; the entry being written is never evicted, and user entries are
        // never evicted here.
        while (self.assistant_bytes() > ASSISTANT_BYTES || self.messages.len() > MAX_MESSAGES)
            && self.evict_oldest_assistant()
        {}
    }
    /// Evict the oldest assistant entries, never the final reply, until
    /// `fits` accepts the history. Returns false when nothing more can go.
    pub fn shrink_until(&mut self, mut fits: impl FnMut(&Self) -> bool) -> bool {
        while !fits(self) {
            let protected = self
                .messages
                .last()
                .filter(|m| m.role == "assistant")
                .map(|m| m.id.clone());
            let Some(i) = self
                .messages
                .iter()
                .position(|m| m.role == "assistant" && Some(&m.id) != protected.as_ref())
            else {
                return false;
            };
            self.messages.remove(i);
            self.assistant_history_omitted = true;
        }
        true
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
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: input,
                confidence: 0.99,
                probabilities: [(
                    serde_json::to_value(input)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    1.0,
                )]
                .into(),
            },
            work: Judgment {
                choice: work,
                confidence: 0.99,
                probabilities: [(
                    serde_json::to_value(work)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    1.0,
                )]
                .into(),
            },
            background: None,
        }
    }
    fn scored(failure: (Failure, f32), input: (Input, f32), work: (Work, f32)) -> Verdict {
        Verdict {
            failure: Judgment {
                choice: failure.0,
                confidence: failure.1,
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: input.0,
                confidence: input.1,
                probabilities: [(
                    serde_json::to_value(input.0)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    1.0,
                )]
                .into(),
            },
            work: Judgment {
                choice: work.0,
                confidence: work.1,
                probabilities: [(
                    serde_json::to_value(work.0)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    1.0,
                )]
                .into(),
            },
            background: None,
        }
    }

    #[test]
    fn probability_gates_agree_across_running_and_completed_turns() {
        use crate::activity::verdict::{Decision, TurnPhase, TurnVerdict, decide};
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/jev-gates.json")).unwrap();
        for case in cases {
            let verdict = TurnVerdict::parse(&case).unwrap();
            let assessment = verdict.assessment.as_ref().unwrap();
            let expected: Action = serde_json::from_value(case["action"].clone()).unwrap();
            let complete = case["authorization_complete"].as_bool().unwrap();
            assert_eq!(assessment.action(complete), expected, "{}", case["name"]);
            let required = case["requires_input"].as_bool().unwrap();
            assert_eq!(assessment.requires_input(), required, "{}", case["name"]);
            assert_eq!(
                decide(TurnPhase::Running, &verdict),
                if required {
                    Decision::AwaitingInput
                } else {
                    Decision::KeepCurrent
                },
                "{}",
                case["name"]
            );
            let replied = match assessment.action(false) {
                Action::AwaitInput => Decision::AwaitingInput,
                Action::Finished => Decision::InferIdle,
                Action::Wait => Decision::ExpectContinuation,
                _ => Decision::KeepCurrent,
            };
            assert_eq!(
                decide(TurnPhase::Replied, &verdict),
                replied,
                "{}",
                case["name"]
            );
            let stored = serde_json::to_value(assessment).unwrap();
            let restored: Verdict = serde_json::from_value(stored).unwrap();
            assert_eq!(&restored, assessment);
            assert_eq!(restored.action(complete), expected);
        }

        // Preserve the cases that were unique in the older decision table.
        assert_eq!(
            scored(
                (Failure::None, 0.99),
                (Input::RedundantRequest, 0.99),
                (Work::Finished, 0.99)
            )
            .action(false),
            Action::Uncertain
        );
        for failure in [(Failure::Unclear, 0.5), (Failure::Other, 0.6)] {
            for work in [Work::Finished, Work::AuthorizedUnfinished] {
                assert_eq!(
                    scored(failure, (Input::None, 0.99), (work, 0.99)).action(true),
                    Action::Uncertain,
                    "{failure:?} {work:?}"
                );
            }
        }
    }

    #[test]
    fn old_saved_assessments_keep_their_action_without_inventing_probabilities() {
        let mut assessment = TurnAssessment::pending(
            "old".into(),
            1,
            0,
            CompletionEvidence {
                stop_reason: "EndTurn".into(),
                diagnostic: None,
            },
        );
        assessment.verdict = Some(verdict(Failure::None, Input::Required, Work::Finished));
        assessment.action = Some(Action::AwaitInput);
        assessment.status = Status::Assessed;
        let mut saved = serde_json::to_value(&assessment).unwrap();
        for axis in ["failure", "input", "work"] {
            saved["verdict"][axis]
                .as_object_mut()
                .unwrap()
                .remove("probabilities");
        }
        let restored: TurnAssessment = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.action, Some(Action::AwaitInput));
        assert!(!restored.needs_classification(0));
        let judgment = restored.verdict.unwrap();
        assert!(!judgment.requires_input());
        assert_eq!(judgment.action(true), Action::Uncertain);
    }

    #[test]
    fn a_missing_background_answer_parses_and_a_bad_one_fails() {
        let mut answers = serde_json::json!({"answers": {
            "failure": {"type": "choice", "choice": "none", "confidence": 0.99, "probabilities": {"none": 1.0, "transient_provider": 0.0, "quota": 0.0, "other": 0.0, "unclear": 0.0}},
            "input": {"type": "choice", "choice": "none", "confidence": 0.99, "probabilities": {"none": 1.0, "redundant_request": 0.0, "required": 0.0, "unclear": 0.0}},
            "work": {"type": "choice", "choice": "finished", "confidence": 0.99, "probabilities": {"finished": 1.0, "authorized_unfinished": 0.0, "waiting": 0.0, "unclear": 0.0}},
        }});
        assert!(Verdict::parse(&answers).unwrap().background.is_none());
        answers["answers"]["background"] = serde_json::json!({"type": "choice", "choice": "unneeded", "confidence": 0.9, "probabilities": {"needed": 0.0, "unneeded": 1.0, "unclear": 0.0}});
        assert_eq!(
            Verdict::parse(&answers)
                .unwrap()
                .background
                .map(|j| j.choice),
            Some(Background::Unneeded)
        );
        answers["answers"]["background"] =
            serde_json::json!({"type": "choice", "choice": "maybe", "confidence": 0.9});
        assert!(Verdict::parse(&answers).is_err());
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
            probabilities: Default::default(),
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
    // Hard-won: 8cecc1c: a detected question beat a confident quota stop, so the session never resumed after the reset.
    #[test]
    fn explicit_user_handoff_blocks_provider_retry_but_not_quota_recovery() {
        assert_eq!(
            verdict(Failure::TransientProvider, Input::Required, Work::Unclear).action(false),
            Action::AwaitInput
        );
        assert_eq!(
            verdict(Failure::Quota, Input::Required, Work::Unclear).action(true),
            Action::RecoverQuota
        );
        let mut low = verdict(Failure::Quota, Input::Required, Work::Unclear);
        low.failure.confidence = 0.89;
        assert_eq!(low.action(true), Action::AwaitInput);
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
        // One pasted document is kept by its head and tail, not refused.
        context.user(
            "pasted",
            &[ContentBlock::Text(TextContent::new(
                "x".repeat(USER_BYTES + 1),
            ))],
        );
        assert!(context.authorization_complete);
        let pasted = context
            .messages
            .iter()
            .find(|m| m.id == "user:pasted")
            .expect("kept");
        assert!(pasted.text.len() < MESSAGE_HEAD_BYTES + MESSAGE_TAIL_BYTES + 100);
        assert!(pasted.text.contains("bytes omitted from the middle"));
        // Only the user byte budget itself makes the history incomplete.
        for i in 0..6 {
            context.user(
                &format!("paste-{i}"),
                &[ContentBlock::Text(TextContent::new("y".repeat(USER_BYTES)))],
            );
        }
        assert!(!context.authorization_complete);
    }

    // Hard-won: a893e12: recorded Codex goal fixtures failed after the previous total-entry cap rejected long sessions.
    #[test]
    fn long_sessions_evict_assistant_entries_before_refusing_a_user_message() {
        use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
        let mut context = ContextHistory::default();
        // Two hundred exchanges with ten assistant messages each: far past
        // the old 256-entry refusal, still complete.
        for i in 0..200 {
            context.user(
                &format!("u{i}"),
                &[ContentBlock::Text(TextContent::new(format!("step {i}")))],
            );
            for j in 0..10 {
                context.assistant(Some(&format!("a{i}-{j}")), i * 10 + j, "ok");
            }
        }
        assert!(
            context.authorization_complete,
            "hundreds of exchanges stay complete"
        );
        assert!(context.messages.len() <= MAX_MESSAGES);
        assert!(context.assistant_history_omitted);
        assert!(!context.final_reply_omitted);
        assert_eq!(context.user_count(), 200);
        assert!(
            context.messages.iter().any(|m| m.id == "user:u0"),
            "user history is never evicted"
        );
        assert_eq!(
            context.messages.last().map(|m| m.role.as_str()),
            Some("assistant")
        );
        // Only the 257th distinct user message cannot fit.
        for i in 200..MAX_MESSAGES {
            context.user(
                &format!("u{i}"),
                &[ContentBlock::Text(TextContent::new(format!("step {i}")))],
            );
        }
        assert!(context.authorization_complete);
        context.user(
            "u-one-too-many",
            &[ContentBlock::Text(TextContent::new("step 256"))],
        );
        assert!(!context.authorization_complete);
    }

    // Hard-won: a893e12: oversized assessment history was dropped instead of evicting old assistant entries to preserve the reply.
    #[test]
    fn shrinking_to_a_wire_limit_drops_old_assistant_entries_but_keeps_the_reply() {
        use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
        let mut context = ContextHistory::default();
        context.user("u", &[ContentBlock::Text(TextContent::new("do the thing"))]);
        for i in 0..12 {
            context.assistant(Some(&format!("a{i}")), i, &"z".repeat(1000));
        }
        let limit = 5_000;
        assert!(context.shrink_until(|c| serde_json::to_vec(c).unwrap().len() <= limit));
        assert!(serde_json::to_vec(&context).unwrap().len() <= limit);
        assert!(context.assistant_history_omitted);
        assert!(context.messages.iter().any(|m| m.id == "user:u"));
        assert_eq!(context.messages.last().map(|m| m.id.as_str()), Some("a11"));
        assert!(!context.final_reply_omitted);
        assert!(context.evidence().validate().is_ok());
        // Once only the user message and the final reply remain, it gives up.
        assert!(!context.shrink_until(|c| serde_json::to_vec(c).unwrap().len() <= 100));
    }

    #[test]
    fn a_long_final_reply_is_trimmed_rather_than_omitted() {
        use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
        let mut context = ContextHistory::default();
        context.user(
            "u",
            &[ContentBlock::Text(TextContent::new("write the report"))],
        );
        for chunk in 0..40 {
            context.assistant(Some("reply"), chunk, &"r".repeat(1024));
        }
        assert!(!context.final_reply_omitted);
        assert!(context.authorization_complete);
        let reply = context.messages.last().unwrap();
        assert_eq!(reply.role, "assistant");
        assert!(reply.text.contains("bytes omitted from the middle"));
        assert!(context.evidence().validate().is_ok());
    }
}

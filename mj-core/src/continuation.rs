//! Evidence and durable admission for bounded, already-authorized continuation.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_NUDGES: u8 = 3;
pub const USER_BYTES: usize = 32 * 1024;
pub const ASSISTANT_BYTES: usize = 16 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const CONFIDENCE: f64 = 0.90;
pub const QUESTIONS: &str = include_str!("continuation/questions.json");
pub const PROMPT: &str = "Continue the unfinished work already requested by the user, following their latest instructions. This message supplies no new approval or missing information. If a genuine decision, required approval, or external action remains necessary, explain it and stop.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationConfig {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}
impl Default for ContinuationConfig {
    fn default() -> Self {
        Self::enabled()
    }
}
const fn enabled_by_default() -> bool {
    true
}
impl ContinuationConfig {
    pub fn enabled() -> Self {
        Self { enabled: true }
    }
    pub fn is_default(&self) -> bool {
        self.enabled
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationState {
    pub user_command_id: Option<String>,
    pub attempts: u8,
    pub suppressed: bool,
    pub completed_command_id: Option<String>,
    #[serde(default)]
    pub quota_suppressed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_recovery: Option<QuotaRecovery>,
    /// Start ordinal of the turn the harness has open on its own, while no
    /// prompt of ours was in flight when it began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomous_turn_started: Option<u64>,
    /// The self-started turn that `completed_command_id` names, once it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_turn: Option<HarnessCompletion>,
}

/// A turn the harness started and ended on its own. It stands in for a
/// completed command so the same checks and guards apply to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCompletion {
    /// `harness-turn-<start ordinal>`; see [`harness_turn_id`].
    pub id: String,
    /// Ordinal of the start event, which is also the transcript position of
    /// the turn's marker.
    pub start_position: u64,
    pub settled_ordinal: u64,
    pub settled_at_ms: i64,
}

pub fn harness_turn_id(start_ordinal: u64) -> String {
    format!("harness-turn-{start_ordinal}")
}

/// Id prefix of the goal resume Mjolnir submits when a quota recovery comes
/// due for a goal the usage limit stopped. The command is an ordinary
/// `GoalControl { Resume }`; the prefix is what makes the relay guard it as a
/// quota recovery and send the goal's identity with it.
pub const QUOTA_GOAL_RESUME_PREFIX: &str = "quota-goal-resume-";

pub fn is_quota_goal_resume(command_id: &str) -> bool {
    command_id.starts_with(QUOTA_GOAL_RESUME_PREFIX)
}

impl ContinuationState {
    pub fn eligible(&self) -> bool {
        self.user_command_id.is_some()
            && self.completed_command_id.is_some()
            && !self.suppressed
            && self.attempts < MAX_NUDGES
    }
}

/// Durable recovery for one completed turn; a missing deadline records abstention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaRecovery {
    pub user_command_id: String,
    pub completed_command_id: String,
    pub profile_id: String,
    pub reset_at_ms: Option<i64>,
    pub retry_at_ms: Option<i64>,
    pub notice: String,
    pub submitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceMessage {
    pub id: String,
    pub role: String,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationEvidence {
    pub assistant_history_omitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_message: Option<String>,
    pub messages: Vec<EvidenceMessage>,
}
impl ContinuationEvidence {
    pub fn validate(&self) -> Result<()> {
        if let Some(message) = &self.quota_message {
            ensure!(
                !message.trim().is_empty() && message.len() <= ASSISTANT_BYTES,
                "invalid quota evidence"
            );
            if self.messages.is_empty() {
                ensure!(
                    serde_json::to_vec(self)?.len() <= MAX_BODY_BYTES,
                    "quota evidence exceeds byte limit"
                );
                return Ok(());
            }
        }
        ensure!(
            !self.messages.is_empty() && self.messages.len() <= 256,
            "invalid continuation messages"
        );
        let mut user = 0;
        let mut assistant = 0;
        for message in &self.messages {
            ensure!(
                !message.id.is_empty() && message.id.len() <= 256,
                "invalid message id"
            );
            ensure!(
                !message.text.trim().is_empty(),
                "empty continuation message"
            );
            match message.role.as_str() {
                "user" => user += message.text.len(),
                "assistant" => assistant += message.text.len(),
                _ => anyhow::bail!("invalid continuation role"),
            }
        }
        ensure!(
            user > 0 && user <= USER_BYTES && assistant > 0 && assistant <= ASSISTANT_BYTES,
            "incomplete or oversized continuation context"
        );
        ensure!(
            self.messages.last().is_some_and(|m| m.role == "assistant"),
            "missing final assistant reply"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BODY_BYTES,
            "continuation evidence exceeds byte limit"
        );
        Ok(())
    }
    pub fn upstream_body(&self) -> Value {
        json!({"model":"jev-latest", "state":self, "questions":serde_json::from_str::<Value>(QUESTIONS).expect("shared questions")})
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContinuationVerdict {
    pub quota_limit: f64,
    pub unfinished: f64,
    pub no_input_needed: f64,
}
impl ContinuationVerdict {
    pub fn parse(body: &Value) -> Result<Self> {
        fn probability(body: &Value, key: &str) -> Result<f64> {
            let answer = &body["answers"][key];
            ensure!(answer["type"] == "noul", "invalid continuation answer type");
            let value = answer["noul"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("missing continuation probability"))?;
            ensure!(
                value.is_finite() && (0.0..=1.0).contains(&value),
                "invalid continuation probability"
            );
            Ok(value)
        }
        Ok(Self {
            quota_limit: probability(body, "quota_limit")?,
            unfinished: probability(body, "unfinished")?,
            no_input_needed: probability(body, "no_input_needed")?,
        })
    }
    pub fn is_quota_limit(self) -> bool {
        (CONFIDENCE..=1.0).contains(&self.quota_limit)
    }
    pub fn should_continue(self) -> bool {
        !self.is_quota_limit()
            && (CONFIDENCE..=1.0).contains(&self.unfinished)
            && (CONFIDENCE..=1.0).contains(&self.no_input_needed)
    }
}

pub fn prompt_blocks() -> Vec<agent_client_protocol::schema::v1::ContentBlock> {
    vec![agent_client_protocol::schema::v1::ContentBlock::Text(
        agent_client_protocol::schema::v1::TextContent::new(PROMPT),
    )]
}

/// These established producers author prompts on the user's behalf. Their
/// text is never evidence of fresh user authorization or a renewed allowance.
pub fn is_generated_prompt_text(text: &str) -> bool {
    crate::second_opinion::is_control_origin_prompt(text)
}

pub fn is_generated_prompt(command_id: &str) -> bool {
    if crate::relay::is_capacity_retry_command(command_id) {
        return true;
    }
    [
        "auto-continue-",
        "quota-retry-",
        "review-forward-",
        "archive-",
        "subagent-",
    ]
    .iter()
    .any(|prefix| command_id.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_requires_both_valid_high_probabilities() {
        for (a, b, want) in [
            (0.99, 0.99, true),
            (0.89, 1.0, false),
            (1.0, 0.89, false),
            (0.90, 0.90, true),
            (f64::NAN, 1.0, false),
            (1.1, 1.0, false),
        ] {
            assert_eq!(
                ContinuationVerdict {
                    quota_limit: 0.0,
                    unfinished: a,
                    no_input_needed: b
                }
                .should_continue(),
                want
            );
        }
        assert!(
            ContinuationVerdict::parse(
                &json!({"answers":{"unfinished":{"type":"noul","noul":1.0}}})
            )
            .is_err()
        );
    }
    #[test]
    fn confident_quota_preempts_ordinary_continuation_and_invalid_scores_fail_closed() {
        for (score, quota) in [(0.89, false), (0.90, true), (1.0, true)] {
            let verdict = ContinuationVerdict::parse(&json!({"answers": {
                "quota_limit": {"type":"noul", "noul":score},
                "unfinished": {"type":"noul", "noul":1.0},
                "no_input_needed": {"type":"noul", "noul":1.0}
            }}))
            .unwrap();
            assert_eq!(verdict.is_quota_limit(), quota);
            assert_eq!(verdict.should_continue(), !quota);
        }
        for score in [json!(null), json!(-0.1), json!(1.1), json!("0.99")] {
            assert!(
                ContinuationVerdict::parse(&json!({"answers": {
                    "quota_limit": {"type":"noul", "noul":score},
                    "unfinished": {"type":"noul", "noul":1.0},
                    "no_input_needed": {"type":"noul", "noul":1.0}
                }}))
                .is_err()
            );
        }
    }

    #[test]
    fn incomplete_and_oversized_evidence_is_rejected() {
        let mut e = ContinuationEvidence {
            quota_message: None,
            assistant_history_omitted: false,
            messages: vec![
                EvidenceMessage {
                    id: "u".into(),
                    role: "user".into(),
                    text: "Implement it and test it".into(),
                },
                EvidenceMessage {
                    id: "a".into(),
                    role: "assistant".into(),
                    text: "Implemented. Shall I test?".into(),
                },
            ],
        };
        assert!(e.validate().is_ok());
        e.messages[0].text = "x".repeat(USER_BYTES + 1);
        assert!(e.validate().is_err());
        e.messages.remove(0);
        assert!(e.validate().is_err());
    }
}

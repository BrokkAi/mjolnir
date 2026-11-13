//! Evidence and durable admission for bounded, already-authorized continuation.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_NUDGES: u8 = 3;
pub const USER_BYTES: usize = 32 * 1024;
pub const ASSISTANT_BYTES: usize = 16 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const CONFIDENCE: f64 = 0.90;
pub const QUESTIONS: &str = include_str!("continuation/questions.json");
pub const PROMPT: &str = "Continue the unfinished work already requested by the user, following their latest instructions. This message supplies no new approval or missing information. If a genuine decision, required approval, or external action remains necessary, explain it and stop.";
/// Historical sub-agent reminder, retained to exclude old journal prompts from authorization.
pub const PARENT_WAIT_PROMPT_TEXT: &str =
    "One or more sub-agents finished. Call wait to collect their reports.";

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
    /// Legacy protocol-39 snapshot field; no longer affects continuation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_children_since_ms: Option<i64>,
    /// Original user request, or an automatic request retained only for quota recovery.
    pub user_command_id: Option<String>,
    pub attempts: u8,
    pub suppressed: bool,
    pub completed_command_id: Option<String>,
    #[serde(default)]
    pub quota_suppressed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_recovery: Option<QuotaRecovery>,
    /// Legacy pre-revision-24 snapshot field; only used during snapshot upgrade.
    /// The open StoredHarnessTurn now owns this fact.
    #[serde(default, skip_serializing)]
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

const HARNESS_TURN_ID_PREFIX: &str = "harness-turn-";

pub fn harness_turn_id(start_ordinal: u64) -> String {
    format!("{HARNESS_TURN_ID_PREFIX}{start_ordinal}")
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
        self.user_command_id
            .as_deref()
            .is_some_and(|id| !is_generated_prompt(id))
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

pub fn prompt_blocks() -> Vec<agent_client_protocol::v1::schema::ContentBlock> {
    vec![agent_client_protocol::v1::schema::ContentBlock::Text(
        agent_client_protocol::v1::schema::TextContent::new(PROMPT),
    )]
}

/// Whether a harness note was authored by Mjolnir rather than a person.
pub fn is_control_origin_prompt(text: &str) -> bool {
    text.trim_start().starts_with("[HARNESS NOTE:")
}

/// These established producers author prompts on the user's behalf. Their
/// text is never evidence of fresh user authorization or a renewed allowance.
pub fn is_generated_prompt_text(text: &str) -> bool {
    is_control_origin_prompt(text) || text.trim() == PARENT_WAIT_PROMPT_TEXT
}

/// Declares the producers once, so the enum, [`GeneratedPrompt::ALL`] and
/// [`GeneratedPrompt::prefix`] cannot disagree: a variant left out of `ALL`
/// would make its prompts look like the person's.
macro_rules! generated_prompts {
    (
        $(#[doc = $enum_doc:literal])*
        pub enum GeneratedPrompt {
            $($(#[doc = $doc:literal])* $variant:ident => $prefix:literal,)*
        }
    ) => {
        $(#[doc = $enum_doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum GeneratedPrompt {
            $($(#[doc = $doc])* $variant,)*
        }

        impl GeneratedPrompt {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            pub const fn prefix(self) -> &'static str {
                match self {
                    $(Self::$variant => $prefix,)*
                }
            }
        }
    };
}

generated_prompts! {
    /// Every producer of user-role prompts that Mjolnir submits itself rather
    /// than the person typing them. This is the one owner of those command ids:
    /// producers build their ids here and every reader asks [`is_generated_prompt`],
    /// so a new producer cannot be added without being recognized (one
    /// declaration lists every producer).
    ///
    /// A prompt that carries the person's own text (plan-review feedback, image
    /// pastes, startup prompts, `prompt-`, `web-prompt-`) is not generated, and
    /// neither is a command that is not a prompt (`auto-steer-`, a goal resume).
    /// The id strings are stored in journals and transcripts; do not change them.
    pub enum GeneratedPrompt {
        /// Automatic continuation of unfinished work.
        AutoContinue => "auto-continue-",
        /// The retry a quota recovery submits.
        QuotaRetry => "quota-retry-",
        /// Resume a turn whose rejected Claude access token was replaced.
        AuthenticationRetry => "auth-retry-",
        /// Review findings forwarded to the author. No current producer; older
        /// transcripts still carry it.
        ReviewForward => "review-forward-",
        /// Restored archive context. No current producer; older transcripts still
        /// carry it.
        Archive => "archive-",
        /// A child's original assignment from its parent.
        SubagentSpawn => "subagent-spawn-",
        /// A parent's follow-up assignment.
        SubagentInput => "subagent-input-",
        /// Other generated sub-agent traffic, including older wait notices.
        Subagent => "subagent-",
        /// A mailbox event that wakes an idle session.
        MailboxWake => "mailbox-wake-",
        /// The prompt that reminds a child to hand back its report.
        HandbackReminder => "handback-reminder-",
        /// Another session's message delivered as a turn.
        SessionMessageTurn => "session-message-turn-",
        /// A worker's retry after a provider failure.
        ServerRetry => "server-retry-",
        /// Older workers' name for [`Self::ServerRetry`].
        CapacityRetry => "capacity-retry-",
    }
}

impl GeneratedPrompt {
    /// The command id this producer gives a prompt: its prefix and `suffix`.
    pub fn command_id(self, suffix: impl std::fmt::Display) -> String {
        format!("{}{suffix}", self.prefix())
    }

    /// What follows this producer's prefix, when `command_id` carries it.
    pub fn suffix(self, command_id: &str) -> Option<&str> {
        command_id.strip_prefix(self.prefix())
    }

    /// The ordinal a retry id ends in: ASCII digits that fit a `u64`.
    pub fn retry_ordinal(self, command_id: &str) -> Option<u64> {
        let ordinal = self.suffix(command_id)?;
        (!ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit()))
            .then(|| ordinal.parse().ok())
            .flatten()
    }

    /// Retry ids end in the ordinal of the turn they retry, and nothing else
    /// counts as one.
    pub const fn is_retry(self) -> bool {
        matches!(
            self,
            Self::ServerRetry | Self::CapacityRetry | Self::AuthenticationRetry
        )
    }

    fn recognizes(self, command_id: &str) -> bool {
        if self.is_retry() {
            self.retry_ordinal(command_id).is_some()
        } else {
            self.suffix(command_id).is_some()
        }
    }

    /// The producer that submitted the prompt with this command id.
    pub fn of(command_id: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|producer| producer.recognizes(command_id))
    }

    /// Parent assignments are authorization only for the handback check.
    pub fn is_subagent_assignment(command_id: &str) -> bool {
        matches!(
            Self::of(command_id),
            Some(Self::SubagentSpawn | Self::SubagentInput)
        )
    }

    /// Id of another session's input to a sub-agent, delivered as a turn.
    pub fn subagent_input_id(request_id: &str) -> String {
        Self::SubagentInput.command_id(request_id)
    }

    /// Id (and startup group id) of a child's spawn brief.
    pub fn subagent_spawn_id(request_key: &str) -> String {
        Self::SubagentSpawn.command_id(request_key)
    }
}

pub fn is_generated_prompt(command_id: &str) -> bool {
    GeneratedPrompt::of(command_id).is_some()
}

/// Whether a user-role transcript item is a message the person wrote. This is
/// the one place the id and text rules are combined; every reader that asks
/// for "the user's latest message" goes through it. `stable_id` is the item's
/// id (`user:<command id>`, or a bare command id), `None` for a prompt the
/// controller has not yet projected, which can only be the person's own.
pub fn is_user_authored_message(stable_id: Option<&str>, text: &str) -> bool {
    if is_generated_prompt_text(text) {
        return false;
    }
    stable_id.is_none_or(|stable_id| {
        let command_id = stable_id.strip_prefix("user:").unwrap_or(stable_id);
        !is_generated_prompt(command_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The mining script keeps its own copy of the rules, so the two sides
    // can drift without either failing to compile.
    #[test]
    fn jev_extract_script_lists_the_same_generated_prompt_prefixes() {
        let script = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../scripts/jev-scenarios-extract.py"
        ))
        .unwrap();
        let quoted_list = |name: &str| -> Vec<String> {
            let body = script
                .split_once(&format!("\n{name} = ("))
                .unwrap_or_else(|| panic!("{name} missing from the script"))
                .1
                .split_once(')')
                .unwrap()
                .0;
            body.split('"')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect()
        };
        let expected = |keep: fn(&GeneratedPrompt) -> bool| -> Vec<String> {
            GeneratedPrompt::ALL
                .iter()
                .filter(|producer| keep(producer))
                .map(|producer| producer.prefix().to_owned())
                .collect()
        };
        assert_eq!(quoted_list("GENERATED_PROMPT_PREFIXES"), expected(|_| true));
        assert_eq!(
            quoted_list("ORDINAL_PREFIXES")
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            expected(|producer| producer.is_retry())
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }

    #[test]
    fn parent_wait_notice_is_generated_prompt_text() {
        assert!(is_generated_prompt_text(PARENT_WAIT_PROMPT_TEXT));
        assert!(is_generated_prompt_text(&format!(
            " {PARENT_WAIT_PROMPT_TEXT}\n"
        )));
        assert!(!is_generated_prompt_text(
            "One or more sub-agents finished."
        ));
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

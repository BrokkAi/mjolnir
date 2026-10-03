//! Shared turn-review requests, status and recovery data.

use super::lanes::{PriorReviewContext, ReviewTier, UserMessage};
use super::verdict::{ReviewPassEvidence, ReviewVerdict};
use crate::relay::AnalyzeDeltaRepository;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What the driver needs the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRequest {
    /// Ask the worker what changed since these baselines.
    CaptureDelta {
        baselines: BTreeMap<PathBuf, String>,
    },
    /// Start Bifrost's semantic analysis of the captured trees. Only the
    /// extended tier asks for it: it runs alongside the intent analyst, and
    /// the supervisor's prompt embeds its result. The quick tier's reviewer
    /// never reads it, so a quick review never runs it.
    AnalyzeDelta {
        repositories: Vec<AnalyzeDeltaRepository>,
    },
    /// Start the reviewer harness for `role`, with a fresh session when
    /// `fresh` is set: every role of a new review starts without another
    /// role's context.
    StartRole { role: String, fresh: bool },
    /// Send `prompt` to `role` under `command_id`.
    PromptRole {
        role: String,
        command_id: String,
        prompt: String,
    },
    /// Send `prompt` to the primary session under `command_id`.
    PromptPrimary { command_id: String, prompt: String },
    /// Stop one role's process group, keeping its staged profile.
    PauseRole { role: String },
    /// Record these trees, and this transcript ordinal, as reviewed.
    AdvanceBaseline {
        trees: BTreeMap<PathBuf, String>,
        reviewed_through_ordinal: u64,
    },
    /// Keep this verdict as the prior review, so the corrective turn's review
    /// verifies it rather than sweeping the code again.
    RecordPriorReview { prior: PriorReviewContext },
    /// Forget any prior review: this pass consumed it.
    ClearPriorReview,
    /// The review is over; close the pane and release the prompt lock.
    Close,
}

/// Which stage of a review one role is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleState {
    Pending,
    Running,
    Clean,
    Findings,
    Failed,
}

impl RoleState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Clean => "done",
            Self::Findings => "findings",
            Self::Failed => "failed",
        }
    }
}

/// One reviewing agent's row in the review pane. It crosses the daemon's
/// snapshot to every surface, so it serializes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleStatus {
    pub role: String,
    pub label: String,
    pub state: RoleState,
}

/// How a review ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The findings went to the primary agent as a corrective prompt.
    Forwarded,
    /// The user read the findings and kept them.
    Dismissed,
    /// The user stopped the review. The baseline does not advance, so the next
    /// review covers this turn too.
    Cancelled,
    /// Nothing changed, so there was nothing to review.
    NothingToReview,
    /// The workspace had no usable baseline, so this capture becomes one and
    /// review coverage starts from here.
    CoverageStarted,
}

/// Where the review has got to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TurnReviewPhase {
    /// Asking the worker what the turn changed.
    CapturingDelta,
    /// Staging and starting the first reviewing agent.
    LaunchingReviewer,
    /// One or more reviewing agents are working.
    Running {
        roles: Vec<RoleStatus>,
    },
    /// A verdict is on screen, waiting for the user.
    Verdict(ReviewVerdict),
    /// The findings are being handed to the primary session. The review stays
    /// open until the relay durably accepts the corrective prompt. Keeping the
    /// findings and command id here makes a retry idempotent and lossless.
    Forwarding {
        synthesis: String,
        evidence: ReviewPassEvidence,
        command_id: String,
        /// Set when the relay rejected the handoff. The findings remain the
        /// same and the command id is deliberately reused on retry.
        error: Option<String>,
    },
    Resolved(Resolution),
}

/// The quick tier's sole reviewer.
pub const REVIEWER_ROLE: &str = "reviewer";
/// The extended tier's supervisor, which owns the verdict.
pub const SUPERVISOR_ROLE: &str = "supervisor";
/// The extended tier's intent analyst.
pub const INTENT_ROLE: &str = "intent";

/// How every command a turn review sends a reviewing role begins, so a
/// reviewer's running prompt says which kind of review it belongs to.
pub const COMMAND_ID_PREFIX: &str = "turn-review-";

/// Everything about the reviewed turn that is known before the capture lands.
#[derive(Debug, Clone)]
pub struct TurnReviewSeed {
    pub tier: ReviewTier,
    /// The latest real user prompt; earlier requirements remain in the history.
    pub task: String,
    /// All real user messages in chronological order, excluding harness notes.
    pub user_messages: Vec<UserMessage>,
    /// The primary's closing message for the reviewed work.
    pub initial_result: String,
    /// A compact rendering of what the primary did.
    pub trajectory: String,
    /// Baselines the capture is taken against.
    pub baselines: BTreeMap<PathBuf, String>,
    /// The transcript ordinal a completed review advances to.
    pub through_ordinal: u64,
    /// A previous forwarded verdict, when this review follows a correction.
    pub prior_review: Option<PriorReviewContext>,
}

/// Durable information needed to reconcile a corrective prompt after the
/// review host is restarted. The command id is the relay's idempotency key;
/// the captured trees and ordinal are what make a later accepted retry safe
/// to finalize without re-running the reviewer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingForward {
    pub synthesis: String,
    #[serde(default)]
    pub evidence: ReviewPassEvidence,
    pub command_id: String,
    pub trees: BTreeMap<PathBuf, String>,
    pub reviewed_through_ordinal: u64,
    /// Who produced the findings, which decides how the corrective prompt
    /// describes them. Durable so a retried handoff sends the same prompt the
    /// first attempt did. Records written before it existed were all a
    /// supervisor's or a validator's vetted synthesis.
    #[serde(default, skip_serializing_if = "FindingsProvenance::is_vetted")]
    pub provenance: FindingsProvenance,
}

/// Who produced a review's findings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingsProvenance {
    /// A role that vetted other reviewers' reports before concluding: the
    /// extended tier's supervisor.
    #[default]
    Vetted,
    /// The quick tier's one reviewer. Nothing checked its findings before
    /// they reach the primary agent.
    SingleReviewer,
}

impl FindingsProvenance {
    #[must_use]
    pub const fn is_vetted(&self) -> bool {
        matches!(self, Self::Vetted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handoff persisted before provenance was recorded came from a
    /// supervisor or the removed validator, and retries with the vetted note.
    #[test]
    fn a_pending_forward_without_provenance_reads_as_vetted() {
        let stored = r#"{"synthesis":"[P1] a.rs:1 -- broken","command_id":"forward-1","trees":{"/w":"t"},"reviewed_through_ordinal":3}"#;
        let pending: PendingForward = serde_json::from_str(stored).unwrap();
        assert_eq!(pending.provenance, FindingsProvenance::Vetted);
        assert!(
            !serde_json::to_string(&pending)
                .unwrap()
                .contains("provenance")
        );
    }

    #[test]
    fn a_single_reviewer_handoff_keeps_its_provenance() {
        let pending = PendingForward {
            synthesis: "[P2] a.rs:1 -- weak test".to_owned(),
            evidence: ReviewPassEvidence::default(),
            command_id: "forward-2".to_owned(),
            trees: BTreeMap::new(),
            reviewed_through_ordinal: 4,
            provenance: FindingsProvenance::SingleReviewer,
        };
        let stored = serde_json::to_string(&pending).unwrap();
        assert!(
            stored.contains(r#""provenance":"single_reviewer""#),
            "{stored}"
        );
        assert_eq!(
            serde_json::from_str::<PendingForward>(&stored).unwrap(),
            pending
        );
    }
}

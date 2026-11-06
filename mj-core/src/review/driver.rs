//! Shared turn-review requests, status and recovery data.

use super::lanes::UserMessage;
use super::verdict::{ReviewPassEvidence, ReviewVerdict};
use crate::relay::RepoDelta;
use crate::subagent::ReviewOverride;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What the driver needs the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReviewRequest {
    /// Ask the worker what changed since these baselines.
    CaptureDelta {
        baselines: BTreeMap<PathBuf, String>,
    },
    /// Register a fresh reviewer child with this initial prompt.
    StartReviewer {
        role: String,
        command_id: String,
        prompt: String,
    },
    /// Send `prompt` to the primary session under `command_id`.
    PromptPrimary { command_id: String, prompt: String },
    /// Close the reviewer child.
    CloseReviewer { role: String },
    /// Record these trees, and this transcript ordinal, as reviewed.
    AdvanceBaseline {
        trees: BTreeMap<PathBuf, String>,
        reviewed_through_ordinal: u64,
    },
    /// Retired: reviews no longer verify prior findings. Kept so a snapshot
    /// written by an older daemon still decodes; the host ignores it.
    RecordPriorReview {},
    /// Retired with `RecordPriorReview`; ignored.
    ClearPriorReview,
    /// An incomplete findings handoff was accepted, but coverage must stay put.
    /// Clear its durable retry record when closing without advancing a baseline.
    ClearPendingForward,
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
    /// The host projects its authoritative registration onto this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
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

/// The legacy sole-reviewer role, retained for old review records.
pub const REVIEWER_ROLE: &str = "reviewer";

/// How every command a turn review sends a reviewing role begins, so a
/// reviewer's running prompt says which kind of review it belongs to.
pub const COMMAND_ID_PREFIX: &str = "turn-review-";

/// Everything about the reviewed turn that is known before the capture lands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnReviewSeed {
    /// The latest real user prompt; earlier requirements remain in the history.
    pub task: String,
    /// All real user messages in chronological order, excluding harness notes.
    pub user_messages: Vec<UserMessage>,
    /// Baselines the capture is taken against.
    pub baselines: BTreeMap<PathBuf, String>,
    /// The transcript ordinal a completed review advances to.
    pub through_ordinal: u64,
    /// Findings the author declined to fix, for the reviewer prompt. An older
    /// snapshot has none.
    #[serde(default)]
    pub declined: Vec<ReviewOverride>,
}

/// One frozen reviewer slot. Its verdict is the only terminal-state fact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewerOutcomeSlot {
    pub role: String,
    pub command_id: String,
    pub outcome: Option<ReviewVerdict>,
}

/// Durable pure-driver state. Child registration and live identity remain host-owned.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReviewDriverSnapshot {
    pub review_id: String,
    pub seed: TurnReviewSeed,
    pub phase: TurnReviewPhase,
    pub last_verdict: Option<ReviewVerdict>,
    pub deltas: Vec<RepoDelta>,
    pub roles: Vec<ReviewerOutcomeSlot>,
    pub provenance: FindingsProvenance,
    pub sequence: u64,
    /// Exact requests emitted by the current stage, before any side effect.
    pub replay_requests: Vec<ReviewRequest>,
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
    /// first attempt did. Records written before it existed used the legacy
    /// vetted wording.
    #[serde(default, skip_serializing_if = "FindingsProvenance::is_vetted")]
    pub provenance: FindingsProvenance,
}

/// Who produced a review's findings. `Vetted` remains for old pending
/// forwards, whose retry must preserve its original correction note.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingsProvenance {
    /// Legacy review output vetted by the removed multi-reviewer flow.
    #[default]
    Vetted,
    /// The sole current reviewer. Nothing checks its findings before they
    /// reach the primary agent.
    SingleReviewer,
    /// Separate reviewer sessions independently inspected the same target.
    IndependentReviewers,
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
    use crate::review::verdict::LaneOutcome;

    // A snapshot written while reviews still verified prior findings holds the
    // retired requests and the seed's `prior_review`.
    #[test]
    fn a_snapshot_from_the_corrective_pass_era_still_decodes() {
        let request: ReviewRequest = serde_json::from_str(
            r#"{"RecordPriorReview":{"prior":{"synthesis":"[P1] a.rs:1 -- broken"}}}"#,
        )
        .unwrap();
        assert_eq!(request, ReviewRequest::RecordPriorReview {});
        let seed: TurnReviewSeed = serde_json::from_str(
            r#"{"task":"t","user_messages":[],"baselines":{},"through_ordinal":3,"prior_review":{"synthesis":"old"}}"#,
        )
        .unwrap();
        assert!(seed.declined.is_empty());
    }

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

    #[test]
    fn a_legacy_pending_forward_with_lane_evidence_still_loads() {
        let stored = r#"{
            "synthesis":"legacy finding",
            "evidence":{
                "intent_brief":"Goal: add a retry",
                "intent_available":true,
                "lanes":[{"id":"tests","outcome":{"outcome":"completed"}}]
            },
            "command_id":"forward-legacy",
            "trees":{"/w":"tree-2"},
            "reviewed_through_ordinal":7,
            "provenance":"vetted"
        }"#;
        let pending: PendingForward = serde_json::from_str(stored).unwrap();
        assert_eq!(pending.provenance, FindingsProvenance::Vetted);
        assert_eq!(pending.evidence.lanes.len(), 1);
        assert_eq!(pending.evidence.lanes[0].id, "tests");
        assert_eq!(pending.evidence.lanes[0].outcome, LaneOutcome::Completed);
    }
}

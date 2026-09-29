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
    /// Start Bifrost's semantic analysis of the captured trees. It runs
    /// alongside the reviewing agents, because its result is not needed until
    /// findings appear (quick tier) or the supervisor starts (extended).
    AnalyzeDelta {
        repositories: Vec<AnalyzeDeltaRepository>,
    },
    /// Start the reviewer harness for `role`, with a fresh session when
    /// `fresh` is set. The validator is a fresh session on purpose: it must
    /// judge the findings against source, not inherit the reviewer's context.
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
/// The quick tier's validator, which verifies the reviewer's findings.
pub const VALIDATOR_ROLE: &str = "validator";
/// The extended tier's supervisor, which owns the verdict.
pub const SUPERVISOR_ROLE: &str = "supervisor";
/// The extended tier's intent analyst.
pub const INTENT_ROLE: &str = "intent";

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
}

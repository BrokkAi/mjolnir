//! Review data shared by Mjolnir's control surfaces.

use hel::hel_review::driver::{Resolution, RoleState, RoleStatus, TurnReviewPhase};
use hel::hel_review::lanes::ReviewTier;
use hel::hel_review::verdict::ReviewVerdict;

/// What the host tells a surface about one running review.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeReviewView {
    pub session_id: String,
    pub tier: ReviewTier,
    pub phase: TurnReviewPhase,
    pub roles: Vec<RoleStatus>,
    /// What the review is doing, in one line.
    pub status: String,
    /// Present once the review has reached a verdict the user must answer.
    pub verdict: Option<VerdictView>,
}

impl RuntimeReviewView {
    /// Whether progress indicators should move. A verdict and a failed
    /// handoff wait for the user even though they retain an activity label.
    #[must_use]
    pub fn is_working(&self) -> bool {
        matches!(
            self.phase,
            TurnReviewPhase::CapturingDelta
                | TurnReviewPhase::LaunchingReviewer
                | TurnReviewPhase::Running { .. }
                | TurnReviewPhase::Forwarding { error: None, .. }
        )
    }

    /// A compact activity label for session lists and headers. Read typed
    /// state rather than matching the driver's human-facing progress text.
    #[must_use]
    pub fn activity_label(&self) -> Option<&'static str> {
        match &self.phase {
            TurnReviewPhase::Resolved(_) => None,
            TurnReviewPhase::Forwarding { error: None, .. } => Some("Sending findings"),
            TurnReviewPhase::Forwarding { error: Some(_), .. } => Some("Forward failed"),
            TurnReviewPhase::Verdict(verdict) => Some(match verdict {
                ReviewVerdict::Findings { .. } => "Findings",
                ReviewVerdict::Failed { .. } => "Review failed",
                ReviewVerdict::Clean => "Review complete",
            }),
            TurnReviewPhase::Running { roles }
                if roles.iter().any(|role| {
                    role.role == hel::hel_review::driver::VALIDATOR_ROLE
                        && matches!(role.state, RoleState::Pending | RoleState::Running)
                }) =>
            {
                Some("Validating")
            }
            _ => Some("Reviewing"),
        }
    }
}

/// A verdict as a surface renders it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerdictView {
    pub kind: VerdictKind,
    /// The findings, or the failure's reason. Empty for a clean verdict, which
    /// resolves itself and is never on screen.
    pub text: String,
    /// Which resolutions this verdict accepts right now. A surface shows the
    /// rest disabled rather than hiding them, so the buttons do not move.
    pub allowed: Vec<Resolution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Clean,
    Findings,
    Failed,
}

/// The relay session id one reviewing role journals under. The default role
/// keeps the plan reviewer's id, which is the one the worker uses.
#[must_use]
pub fn role_session_id(primary_session_id: &str, role: &str) -> String {
    if role == hel::hel_review::driver::REVIEWER_ROLE {
        format!("{primary_session_id}-reviewer")
    } else {
        format!("{primary_session_id}-review-{role}")
    }
}

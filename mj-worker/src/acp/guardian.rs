//! Worker-side ACP guardian and permission responses.

use super::*;
use mj_core::guardian::{
    Assessment, GUARDIAN_REVIEW_TIMEOUT, GUARDIAN_TRANSCRIPT_BYTES, GuardianReviewRequest, Outcome,
    PriorGuardianReview, parse_assessment, planned_action, reviewer_instructions_acp,
};

mod reviewer;
mod reviewer_profile;

pub(super) use mj_core::guardian::{ask_person_preface, denied_escalation_preface};
pub(super) use reviewer::Guardian;

/// Sent in the same reviewer session when a reply carried no verdict.
const VERDICT_REMINDER: &str = "Your reply did not contain the required verdict. Reply with only the strict JSON verdict described in the output contract, and nothing else.";

/// What the reviewer is shown about one permission request.
#[derive(Debug, Clone)]
pub(super) struct ReviewEvidence {
    pub(super) request: GuardianReviewRequest,
    /// Their identities, to tell a new message from the history's window
    /// moving.
    pub(super) user_message_ids: Vec<String>,
    trusted_user_input_ids: Vec<String>,
}

impl ReviewEvidence {
    /// Collect the evidence for `request` from the session's turn context.
    pub(super) fn collect(
        harness: HarnessKind,
        turn_context: &mj_transcript::turn_context::TurnContext,
        tool_call_id: &str,
        action: &serde_json::Value,
        project_instructions: Option<String>,
    ) -> Self {
        let authorization = turn_context.authorization_context();
        let person = authorization
            .iter()
            .flat_map(|history| &history.messages)
            .filter(|message| message.role == "user");
        let user_instructions = person.clone().map(|message| message.text.clone()).collect();
        let user_message_ids = person.map(|message| message.id.clone()).collect();
        // No durable history means nothing the person wrote can be vouched
        // for; the reviewer is told so.
        let user_instructions_omitted = authorization
            .as_ref()
            .is_none_or(|history| !history.authorization_complete);
        let trusted_user_inputs = turn_context.trusted_user_inputs();
        let trusted_user_input_ids = trusted_user_inputs
            .iter()
            .map(|input| input.id.clone())
            .collect();
        let (parent_instructions, parent_instructions_omitted) =
            turn_context.delegated_parent_instructions();
        Self {
            request: GuardianReviewRequest {
                request_id: tool_call_id.to_owned(),
                harness,
                session_id: turn_context.session_id(),
                tool_call_id: tool_call_id.to_owned(),
                user_instructions,
                user_instructions_omitted,
                project_instructions,
                transcript: turn_context.render_transcript(GUARDIAN_TRANSCRIPT_BYTES),
                action: action.clone(),
                trusted_user_inputs,
                parent_instructions,
                parent_instructions_omitted,
                prior_reviews: turn_context.prior_guardian_reviews(),
                retry_reason: None,
            },
            user_message_ids,
            trusted_user_input_ids,
        }
    }

    /// Detect a new instruction or answer, including a grant or a revocation.
    /// Republishing the same retained evidence does not trigger another review.
    pub(super) fn authorization_changed_since(&self, earlier: &Self) -> bool {
        self.user_message_ids
            .iter()
            .any(|id| !earlier.user_message_ids.contains(id))
            || self
                .trusted_user_input_ids
                .iter()
                .any(|id| !earlier.trusted_user_input_ids.contains(id))
            || self.request.parent_instructions.iter().any(|input| {
                !earlier
                    .request
                    .parent_instructions
                    .iter()
                    .any(|previous| previous.id == input.id)
            })
    }
}

/// How the guardian answered one permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GuardianDecision {
    Allow {
        review: PriorGuardianReview,
    },
    Deny {
        rationale: String,
        review: PriorGuardianReview,
    },
    /// The guardian could not decide; the person answers the ordinary form,
    /// which shows this reason.
    AskPerson {
        reason: String,
    },
}

/// Allow this one call, never as a rule the harness would remember: the
/// request's one-time allow, or `None` when it offers none.
pub(super) fn allow_once_response(
    request: &RequestPermissionRequest,
) -> Option<RequestPermissionResponse> {
    request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
        .map(|option| {
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            ))
        })
}

/// Refuse this one call: the request's one-time rejection, never a
/// rejection the harness would remember as a rule, or a cancelled request
/// when it offers none. Used to refuse every permission request the
/// read-only ACP guardian reviewer itself raises.
pub(super) fn rejection_response(request: &RequestPermissionRequest) -> RequestPermissionResponse {
    request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::RejectOnce)
        .map_or_else(
            || RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
            |option| {
                RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                    SelectedPermissionOutcome::new(option.option_id.clone()),
                ))
            },
        )
}

#[cfg(test)]
#[path = "guardian_tests.rs"]
mod tests;

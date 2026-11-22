//! Worker-side ACP guardian and permission responses.

use super::*;
use mj_core::guardian::{
    Assessment, GUARDIAN_REVIEW_TIMEOUT, GUARDIAN_TRANSCRIPT_BYTES, GuardianReviewRequest, Outcome,
    parse_assessment, planned_action, reviewer_instructions_acp,
};

mod reviewer;
mod reviewer_profile;

pub(super) use mj_core::guardian::{ask_person_preface, denial_notice};
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
            },
            user_message_ids,
        }
    }

    /// Whether the person wrote something this evidence does not hold, such
    /// as a new grant or a revocation. The history's window moving on, or the
    /// relay republishing it, is not new authorization.
    pub(super) fn person_wrote_since(&self, earlier: &Self) -> bool {
        self.user_message_ids
            .iter()
            .any(|id| !earlier.user_message_ids.contains(id))
    }
}

/// How the guardian answered one permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GuardianDecision {
    Allow,
    Deny {
        rationale: String,
    },
    /// Review infrastructure failed. Finish the request with an ACP error.
    Failed {
        reason: String,
    },
    /// A policy decision handed this request to the person.
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
/// when it offers none. Both the guardian's denials and every request the
/// reviewer itself raises are answered this way.
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

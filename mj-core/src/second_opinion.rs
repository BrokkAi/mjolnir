//! Plan second-opinion workflow. Reviewer selection is shared with turn review.

use serde::{Deserialize, Serialize};

/// Opening marker of every prompt Hel generates for a review.
///
/// These are control-origin records: Hel wrote them, not the user, and the
/// transcript says so rather than attributing them to whoever asked for the
/// review. Generating and recognizing them share this one marker.
pub const HARNESS_NOTE_MARKER: &str = "[HARNESS NOTE:";

/// Whether this prompt text is one Hel generated rather than one a person
/// typed.
#[must_use]
pub fn is_control_origin_prompt(text: &str) -> bool {
    text.trim_start().starts_with(HARNESS_NOTE_MARKER)
}

/// The harness note that asks the primary for the context a reviewer needs.
///
/// The plan itself is already captured, so the primary is asked only for what
/// Hel cannot reconstruct: what the user wanted and why the plan looks as it
/// does.
pub const PRIMARY_CONTEXT_REQUEST: &str = "[HARNESS NOTE: the user wants to get a second opinion before approving this plan. I have captured your plan proposal; you do not need to repeat that. What I need from you is a summary of what the user asked for and decisions made, so that I can give the other agent appropriate context.]";

/// Builds the reviewer's opening prompt from the primary's summary and the
/// proposal exactly as the primary wrote it.
#[must_use]
pub fn review_request(summary: &str, proposal: &str) -> String {
    format!(
        "[HARNESS NOTE: another agent produced the plan below and its user wants a second opinion before approving it. Critique the plan: say what it gets right, what it gets wrong, and what it misses. Do not implement it and do not edit any files.]\n\n\
         <context_from_the_planning_agent>\n{summary}\n</context_from_the_planning_agent>\n\n\
         <plan_under_review>\n{proposal}\n</plan_under_review>"
    )
}

/// Wraps a reviewer's answer for the primary. The answer travels unchanged;
/// only the note around it is Hel's.
#[must_use]
pub fn transfer_note(answer: &str) -> String {
    format!(
        "[HARNESS NOTE: the user asked another agent to review your plan. Its review follows verbatim. Weigh it, then revise your plan. You are still in plan mode; do not start implementing.]\n\n\
         <second_opinion>\n{answer}\n</second_opinion>"
    )
}

/// Tells the primary to implement the plan it proposed, quoted back so the
/// instruction cannot drift from what the user approved.
#[must_use]
pub fn implement_original_note(proposal: &str) -> String {
    format!(
        "[HARNESS NOTE: the user reviewed a second opinion and chose to implement your original plan unchanged. Implement exactly the plan below.]\n\n\
         <approved_plan>\n{proposal}\n</approved_plan>"
    )
}

/// Work the review workflow needs the transport to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowRequest {
    /// Send `prompt` to the primary session under `command_id`.
    PromptPrimary { command_id: String, prompt: String },
    /// Send `prompt` to the reviewer session under `command_id`.
    PromptReviewer { command_id: String, prompt: String },
    /// Cancel any reviewer turn in flight and pause its process, keeping its
    /// native session and transcript for the next review.
    PauseReviewer,
    /// Put a Hel-owned decision dialog back in front of the user. The harness's
    /// own approval was consumed to gather context, so only Hel can offer the
    /// captured proposal again.
    RestoreDecision {
        proposal_id: String,
        proposal: String,
    },
}

/// How far one review has got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum ReviewStage {
    /// The primary was asked to summarize what led to the plan.
    GatheringContext { command_id: String },
    /// The reviewer is reading the plan.
    Reviewing { command_id: String },
    /// The reviewer's answer is complete and can be transferred.
    Answered { answer: String },
    /// The reviewer's answer went to the primary; the split is closing.
    Transferred,
    /// The primary was told to implement the captured plan.
    Implementing,
    /// The user backed out. Hel owes them a decision on the captured proposal.
    Cancelled,
}

/// One second-opinion review, from the decision that started it to the command
/// that ends it.
///
/// Every step names the command it is waiting on, and a completion for any
/// other command is ignored. A reconnect that replays an already-applied
/// completion therefore cannot send feedback twice or start a second reviewer
/// turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewWorkflow {
    proposal_id: String,
    proposal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    stage: ReviewStage,
}

impl ReviewWorkflow {
    /// Starts a review of `proposal`, asking the primary for context under
    /// `command_id`.
    ///
    /// The caller resolves the harness's own plan decision first, through the
    /// dialect bridge, so this prompt reaches an idle planning session.
    #[must_use]
    pub fn start(
        proposal_id: impl Into<String>,
        proposal: impl Into<String>,
        command_id: impl Into<String>,
    ) -> (Self, WorkflowRequest) {
        let command_id = command_id.into();
        let workflow = Self {
            proposal_id: proposal_id.into(),
            proposal: proposal.into(),
            summary: None,
            stage: ReviewStage::GatheringContext {
                command_id: command_id.clone(),
            },
        };
        let request = WorkflowRequest::PromptPrimary {
            command_id,
            prompt: PRIMARY_CONTEXT_REQUEST.to_owned(),
        };
        (workflow, request)
    }

    #[must_use]
    pub fn stage(&self) -> &ReviewStage {
        &self.stage
    }

    #[must_use]
    pub fn proposal(&self) -> &str {
        &self.proposal
    }

    #[must_use]
    pub fn proposal_id(&self) -> &str {
        &self.proposal_id
    }

    /// The primary's summary, once it has arrived.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Whether the split's transfer action is currently allowed. It is not
    /// until the reviewer's latest turn has a complete answer.
    #[must_use]
    pub fn can_transfer(&self) -> bool {
        matches!(self.stage, ReviewStage::Answered { .. })
    }

    /// Whether the workflow has ended and its split should close.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(
            self.stage,
            ReviewStage::Transferred | ReviewStage::Implementing | ReviewStage::Cancelled
        )
    }

    /// Records the primary's answer to the context request and prompts the
    /// reviewer under `reviewer_command_id`.
    ///
    /// A completion for any other command is ignored, so replaying an old
    /// completion after a reconnect starts no second reviewer turn.
    pub fn primary_context_completed(
        &mut self,
        command_id: &str,
        summary: impl Into<String>,
        reviewer_command_id: impl Into<String>,
    ) -> Option<WorkflowRequest> {
        let ReviewStage::GatheringContext {
            command_id: awaited,
        } = &self.stage
        else {
            return None;
        };
        if awaited != command_id {
            return None;
        }
        let summary = summary.into();
        let prompt = review_request(&summary, &self.proposal);
        let reviewer_command_id = reviewer_command_id.into();
        self.summary = Some(summary);
        self.stage = ReviewStage::Reviewing {
            command_id: reviewer_command_id.clone(),
        };
        Some(WorkflowRequest::PromptReviewer {
            command_id: reviewer_command_id,
            prompt,
        })
    }

    /// Records the reviewer's completed answer, which enables transfer.
    pub fn reviewer_turn_completed(&mut self, command_id: &str, answer: impl Into<String>) {
        let ReviewStage::Reviewing {
            command_id: awaited,
        } = &self.stage
        else {
            return;
        };
        if awaited != command_id {
            return;
        }
        self.stage = ReviewStage::Answered {
            answer: answer.into(),
        };
    }

    /// Sends the reviewer's latest answer to the primary, unchanged, under a
    /// harness note. Plan mode stays active: the revised plan is the primary's
    /// to produce.
    pub fn transfer(&mut self, command_id: impl Into<String>) -> Vec<WorkflowRequest> {
        let ReviewStage::Answered { answer } = &self.stage else {
            return Vec::new();
        };
        let prompt = transfer_note(answer);
        self.stage = ReviewStage::Transferred;
        vec![
            WorkflowRequest::PromptPrimary {
                command_id: command_id.into(),
                prompt,
            },
            WorkflowRequest::PauseReviewer,
        ]
    }

    /// Tells the primary to implement the captured proposal. No reviewer
    /// feedback travels with it.
    pub fn implement_original(&mut self, command_id: impl Into<String>) -> Vec<WorkflowRequest> {
        if self.finished() {
            return Vec::new();
        }
        let prompt = implement_original_note(&self.proposal);
        self.stage = ReviewStage::Implementing;
        vec![
            WorkflowRequest::PromptPrimary {
                command_id: command_id.into(),
                prompt,
            },
            WorkflowRequest::PauseReviewer,
        ]
    }

    /// Backs out without transferring anything, and asks for the captured
    /// proposal to be put back in front of the user.
    pub fn cancel(&mut self) -> Vec<WorkflowRequest> {
        if self.finished() {
            return Vec::new();
        }
        self.stage = ReviewStage::Cancelled;
        vec![
            WorkflowRequest::PauseReviewer,
            WorkflowRequest::RestoreDecision {
                proposal_id: self.proposal_id.clone(),
                proposal: self.proposal.clone(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn started() -> (ReviewWorkflow, WorkflowRequest) {
        ReviewWorkflow::start("plan-review-1", "1. Read\n2. Change", "context-1")
    }

    #[test]
    fn a_review_asks_the_primary_for_context_before_it_prompts_the_reviewer() {
        let (mut workflow, request) = started();
        assert_eq!(
            request,
            WorkflowRequest::PromptPrimary {
                command_id: "context-1".into(),
                prompt: PRIMARY_CONTEXT_REQUEST.to_owned(),
            }
        );
        // The captured plan travels with the workflow, so the primary is never
        // asked to repeat it.
        assert!(PRIMARY_CONTEXT_REQUEST.contains("you do not need to repeat that"));
        assert!(!workflow.can_transfer());

        let prompt = workflow
            .primary_context_completed("context-1", "The user asked for X", "review-1")
            .expect("the summary starts the reviewer's turn");
        let WorkflowRequest::PromptReviewer { command_id, prompt } = prompt else {
            panic!("the reviewer is prompted next");
        };
        assert_eq!(command_id, "review-1");
        assert!(prompt.contains("The user asked for X"));
        // The proposal reaches the reviewer exactly as the primary wrote it.
        assert!(prompt.contains("1. Read\n2. Change"));
        assert!(prompt.contains("Do not implement it"));
        assert_eq!(workflow.summary(), Some("The user asked for X"));
        assert!(!workflow.can_transfer());
    }

    #[test]
    fn transfer_waits_for_a_complete_reviewer_answer() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        assert!(!workflow.can_transfer());
        assert!(workflow.transfer("transfer-1").is_empty());

        workflow.reviewer_turn_completed("review-1", "The plan misses error handling.");
        assert!(workflow.can_transfer());

        let requests = workflow.transfer("transfer-1");
        let [
            WorkflowRequest::PromptPrimary { command_id, prompt },
            WorkflowRequest::PauseReviewer,
        ] = requests.as_slice()
        else {
            panic!("transfer prompts the primary and pauses the reviewer");
        };
        assert_eq!(command_id, "transfer-1");
        // The answer travels unchanged inside Hel's note.
        assert!(prompt.contains("The plan misses error handling."));
        assert!(prompt.contains("do not start implementing"));
        assert!(workflow.finished());
    }

    #[test]
    fn a_replayed_completion_never_sends_feedback_twice() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        // The same context completion arriving again after a reconnect must
        // not start a second reviewer turn.
        assert!(
            workflow
                .primary_context_completed("context-1", "context", "review-2")
                .is_none()
        );
        assert_eq!(
            workflow.stage(),
            &ReviewStage::Reviewing {
                command_id: "review-1".into()
            }
        );

        workflow.reviewer_turn_completed("review-1", "answer");
        assert!(!workflow.transfer("transfer-1").is_empty());
        assert!(workflow.transfer("transfer-2").is_empty());
        assert!(workflow.implement_original("implement-1").is_empty());
        assert!(workflow.cancel().is_empty());
    }

    #[test]
    fn a_completion_for_another_command_is_ignored() {
        let (mut workflow, _) = started();
        assert!(
            workflow
                .primary_context_completed("some-other-command", "stale", "review-1")
                .is_none()
        );
        assert_eq!(workflow.summary(), None);

        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("a-different-turn", "stale answer");
        assert!(!workflow.can_transfer());
    }

    #[test]
    fn implementing_the_original_carries_the_captured_plan_and_no_feedback() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("review-1", "I would restructure the whole thing.");

        let requests = workflow.implement_original("implement-1");
        let [
            WorkflowRequest::PromptPrimary { prompt, .. },
            WorkflowRequest::PauseReviewer,
        ] = requests.as_slice()
        else {
            panic!("implementing prompts the primary and pauses the reviewer");
        };
        assert!(prompt.contains("1. Read\n2. Change"));
        assert!(!prompt.contains("I would restructure the whole thing."));
        assert!(workflow.finished());
    }

    #[test]
    fn cancelling_transfers_nothing_and_asks_for_the_decision_back() {
        let (mut workflow, _) = started();
        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("review-1", "unwanted feedback");

        assert_eq!(
            workflow.cancel(),
            vec![
                WorkflowRequest::PauseReviewer,
                WorkflowRequest::RestoreDecision {
                    proposal_id: "plan-review-1".into(),
                    proposal: "1. Read\n2. Change".into(),
                },
            ]
        );
        assert!(workflow.finished());
    }
}

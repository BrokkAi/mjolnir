use super::*;
use crate::verdict::ReviewPassEvidence;
use mj_core::review::lanes::UserMessage;

fn seed() -> TurnReviewSeed {
    TurnReviewSeed {
        task: "add a retry".to_string(),
        user_messages: vec![UserMessage::prompt("add a retry")],
        baselines: BTreeMap::from([(PathBuf::from("/w/app"), "base-tree".to_string())]),
        through_ordinal: 12,
        prior_review: None,
    }
}

fn changed_delta() -> Vec<RepoDelta> {
    vec![RepoDelta {
        root: PathBuf::from("/w/app"),
        baseline_tree: Some("base-tree".to_string()),
        current_tree: "new-tree".to_string(),
        patch: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n+retry\n".to_string(),
        diffstat: "1 file changed, 1 insertion(+)".to_string(),
        changed_lines: 1,
        files: vec![mj_core::relay::FileLineChange {
            path: "src/lib.rs".to_string(),
            insertions: 1,
            ..Default::default()
        }],
    }]
}

fn empty_delta() -> Vec<RepoDelta> {
    vec![RepoDelta {
        root: PathBuf::from("/w/app"),
        baseline_tree: Some("base-tree".to_string()),
        current_tree: "base-tree".to_string(),
        patch: String::new(),
        diffstat: "0 files changed".to_string(),
        changed_lines: 0,
        files: Vec::new(),
    }]
}

/// The command a role was just prompted under, from the requests it
/// produced.
fn prompted(requests: &[ReviewRequest], role: &str) -> String {
    requests
        .iter()
        .find_map(|request| match request {
            ReviewRequest::PromptRole {
                role: prompted,
                command_id,
                ..
            } if prompted == role => Some(command_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{role} was not prompted in {requests:?}"))
}

fn prompt_text(requests: &[ReviewRequest], role: &str) -> String {
    requests
        .iter()
        .find_map(|request| match request {
            ReviewRequest::PromptRole {
                role: prompted,
                prompt,
                ..
            } if prompted == role => Some(prompt.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{role} was not prompted in {requests:?}"))
}

/// Drives a review to the point where the reviewer has been prompted.
fn running() -> (TurnReviewDriver, String) {
    let (mut driver, requests) = TurnReviewDriver::start(seed());
    assert_eq!(
        requests,
        vec![ReviewRequest::CaptureDelta {
            baselines: seed().baselines
        }]
    );
    let requests = driver.delta_captured(changed_delta());
    assert_eq!(
        requests,
        vec![ReviewRequest::StartRole {
            role: REVIEWER_ROLE.to_string(),
            fresh: true
        }],
        "the review starts its one reviewer"
    );
    let requests = driver.role_started(REVIEWER_ROLE);
    let command_id = prompted(&requests, REVIEWER_ROLE);
    let prompt = prompt_text(&requests, REVIEWER_ROLE);
    assert!(prompt.contains("git -C /w/app diff --no-ext-diff base-tree new-tree"));
    assert!(
        !prompt.contains("+retry"),
        "the prompt does not embed the diff"
    );
    assert!(prompt.contains("add a retry"));
    (driver, command_id)
}

#[test]
fn a_turn_that_changed_nothing_records_a_baseline_and_reviews_nothing() {
    let (mut driver, _) = TurnReviewDriver::start(seed());
    let requests = driver.delta_captured(empty_delta());
    assert_eq!(
        requests,
        vec![
            ReviewRequest::AdvanceBaseline {
                trees: BTreeMap::from([(PathBuf::from("/w/app"), "base-tree".to_string())]),
                reviewed_through_ordinal: 12,
            },
            ReviewRequest::Close,
        ]
    );
    assert!(driver.finished());
    assert_eq!(
        driver.phase(),
        &TurnReviewPhase::Resolved(Resolution::NothingToReview)
    );
}

// Hard-won: 2a11e18a80: a workspace without a baseline was misleadingly labeled unchanged.
#[test]
fn a_workspace_with_no_baseline_starts_coverage_rather_than_reviewing_nothing() {
    let mut seed = seed();
    seed.baselines.clear();
    let (mut driver, _) = TurnReviewDriver::start(seed);
    let requests = driver.delta_captured(vec![RepoDelta {
        root: PathBuf::from("/w/app"),
        baseline_tree: None,
        current_tree: "first-tree".to_string(),
        patch: String::new(),
        diffstat: "0 files changed".to_string(),
        changed_lines: 0,
        files: Vec::new(),
    }]);
    assert_eq!(
        driver.phase(),
        &TurnReviewPhase::Resolved(Resolution::CoverageStarted),
        "the user is told coverage started, not that their turn changed nothing"
    );
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. }))
    );
}

#[test]

fn a_clean_review_runs_one_reviewer_and_advances_the_baseline_itself() {
    let (mut driver, command_id) = running();
    let requests = driver.role_turn_completed(&command_id, r#"{"findings":[]}"#);
    assert_eq!(
        requests,
        vec![
            ReviewRequest::PauseRole {
                role: REVIEWER_ROLE.to_string()
            },
            ReviewRequest::AdvanceBaseline {
                trees: BTreeMap::from([(PathBuf::from("/w/app"), "new-tree".to_string())]),
                reviewed_through_ordinal: 12,
            },
            ReviewRequest::Close,
        ],
        "a clean reviewer releases the turn itself"
    );
    assert!(driver.finished());
    assert_eq!(
        driver.last_verdict(),
        Some(&ReviewVerdict::Clean),
        "the clean verdict remains available for the close notice"
    );
}

#[test]
fn findings_are_the_verdict_without_a_second_pass() {
    let (mut driver, command_id) = running();
    let requests =
        driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    assert_eq!(
        requests,
        vec![ReviewRequest::PauseRole {
            role: REVIEWER_ROLE.to_string()
        }],
        "findings reap the reviewer and start nothing else"
    );
    assert!(driver.can_forward());
    assert!(!driver.finished(), "findings wait to be forwarded");
    assert_eq!(
        driver.last_verdict(),
        Some(&ReviewVerdict::Findings {
            synthesis: "[P1] src/lib.rs:1 -- unbounded retry loop".to_string(),
            evidence: ReviewPassEvidence::default(),
        })
    );
    assert_eq!(
        driver.roles(),
        vec![RoleStatus {
            role: REVIEWER_ROLE.to_string(),
            label: "Reviewer".to_string(),
            state: RoleState::Findings,
        }],
        "a verdict keeps the completed role state available to surfaces"
    );
}

#[test]
fn an_empty_report_fails_the_review_and_leaves_the_baseline_alone() {
    let (mut driver, command_id) = running();
    let requests = driver.role_turn_completed(&command_id, "  \n ");
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. })),
        "a failed review never advances the baseline: {requests:?}"
    );
    let ReviewVerdict::Failed { reason } = driver.verdict().expect("a verdict is on screen") else {
        panic!(
            "an empty report must fail the review, got {:?}",
            driver.phase()
        );
    };
    assert!(reason.contains("empty report"), "{reason}");
    assert!(!driver.can_forward());
}

/// The primary is told what it is getting: one reviewer's findings that
/// nobody checked, which it verifies against source before acting.
#[test]
fn a_single_reviewer_forward_says_the_findings_are_unverified() {
    let (mut driver, command_id) = running();
    driver.role_turn_completed(&command_id, "[P2] src/lib.rs:1 -- weak test");
    let requests = driver.forward("test-forward-command".to_owned());
    let [ReviewRequest::PromptPrimary { prompt, .. }] = requests.as_slice() else {
        panic!("forward submits one primary prompt, got {requests:?}");
    };
    assert!(prompt.starts_with("[HARNESS NOTE: an independent review"));
    assert!(prompt.contains("nobody has checked them"), "{prompt}");
    assert!(prompt.contains("not independently verified"), "{prompt}");
    assert!(!prompt.contains("validated by"), "{prompt}");
    assert!(prompt.contains("[P2] src/lib.rs:1 -- weak test"));
    assert_eq!(
        driver.pending_forward().map(|pending| pending.provenance),
        Some(FindingsProvenance::SingleReviewer)
    );
}

#[test]
fn cancelling_leaves_the_baseline_so_the_next_review_covers_both_turns() {
    let (mut driver, _) = running();
    let requests = driver.cancel();
    assert_eq!(
        requests,
        vec![
            ReviewRequest::PauseRole {
                role: REVIEWER_ROLE.to_string()
            },
            ReviewRequest::Close
        ]
    );
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. })),
        "cancel must not advance the baseline"
    );
    assert_eq!(
        driver.phase(),
        &TurnReviewPhase::Resolved(Resolution::Cancelled)
    );
    assert!(driver.cancel().is_empty(), "cancelling twice is inert");
}

#[test]
fn a_rejected_forward_keeps_findings_and_retries_the_same_command() {
    let (mut driver, command_id) = running();
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");

    let first = driver.forward("test-forward-command".to_owned());
    let ReviewRequest::PromptPrimary { command_id, prompt } = &first[0] else {
        panic!("forward starts one primary submission: {first:?}");
    };
    let command_id = command_id.clone();
    let prompt = prompt.clone();
    assert!(
        driver.forward("test-forward-command".to_owned()).is_empty(),
        "a pending handoff cannot race itself"
    );

    assert!(
        driver
            .forward_failed("primary rejected the prompt")
            .is_empty()
    );
    assert!(driver.can_forward(), "a rejected handoff is retryable");
    assert!(matches!(
        driver.phase(),
        TurnReviewPhase::Forwarding {
            error: Some(reason),
            ..
        } if reason == "primary rejected the prompt"
    ));

    let retry = driver.forward("different-id-must-be-ignored".to_owned());
    assert_eq!(
        retry,
        vec![ReviewRequest::PromptPrimary { command_id, prompt }],
        "retry reuses the exact command and corrective prompt"
    );
    assert!(
        driver
            .forward_succeeded()
            .iter()
            .any(|request| matches!(request, ReviewRequest::RecordPriorReview { .. }))
    );
}

#[test]
fn a_late_acceptance_after_rejection_still_resolves_the_same_handoff() {
    let (mut driver, command_id) = running();
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    let first = driver.forward("test-forward-command".to_owned());
    let ReviewRequest::PromptPrimary { command_id, .. } = &first[0] else {
        panic!("forward must submit the primary prompt: {first:?}");
    };
    let command_id = command_id.clone();
    driver.forward_failed("relay response was ambiguous");

    let accepted = driver.forward_succeeded();
    assert!(
        accepted
            .iter()
            .any(|request| matches!(request, ReviewRequest::RecordPriorReview { .. }))
    );
    assert!(driver.finished());
    assert_eq!(
        driver.pending_forward().map(|pending| pending.command_id),
        None,
        "the resolved driver no longer exposes a pending command"
    );
    assert!(!command_id.is_empty());
}

#[test]
fn an_interrupted_handoff_retries_with_its_durable_command_id() {
    let (mut driver, command_id) = running();
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    driver.forward("test-forward-command".to_owned());
    let pending = driver
        .pending_forward()
        .expect("forwarding state is durable");
    assert_eq!(
        pending.provenance,
        FindingsProvenance::SingleReviewer,
        "the handoff remembers who produced the findings, so a retry sends the same note"
    );

    let (mut resumed, initial) = TurnReviewDriver::resume_forward(seed(), pending.clone());
    assert!(
        initial.is_empty(),
        "recovery starts at the handoff boundary"
    );
    assert_eq!(
        resumed.forward("unused-new-command".to_owned()),
        vec![ReviewRequest::PromptPrimary {
            command_id: pending.command_id,
            prompt: correction_note(&pending.synthesis, pending.provenance),
        }]
    );
}

#[test]
fn a_pending_forward_cannot_be_cancelled_before_the_relay_acknowledges_it() {
    let (mut driver, command_id) = running();
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    driver.forward("test-forward-command".to_owned());

    assert!(driver.cancel().is_empty());
    assert!(!driver.finished());
}

#[test]
fn a_completion_for_another_command_is_ignored() {
    let (mut driver, _) = running();
    assert!(
        driver
            .role_turn_completed("turn-review-reviewer-999", r#"{"findings":[]}"#)
            .is_empty(),
        "a replayed completion for another command cannot advance the review"
    );
    assert!(!driver.finished());
}

#[test]
fn a_request_failure_ends_as_a_dismissable_failed_verdict() {
    let (mut driver, _) = running();
    let requests = driver.request_failed("the reviewer could not start");
    assert_eq!(
        requests,
        vec![ReviewRequest::PauseRole {
            role: REVIEWER_ROLE.to_string()
        }]
    );
    assert!(matches!(
        driver.verdict(),
        Some(ReviewVerdict::Failed { .. })
    ));
    let requests = driver.dismiss();
    assert_eq!(requests, vec![ReviewRequest::Close]);
    assert!(driver.finished());
}

#[test]
fn a_verification_pass_consumes_the_prior_review_when_it_resolves() {
    let mut seed = seed();
    seed.prior_review = Some(PriorReviewContext {
        synthesis: "[P1] src/lib.rs:1 -- no bound".to_string(),
        evidence: ReviewPassEvidence::default(),
    });
    let (mut driver, _) = TurnReviewDriver::start(seed);
    driver.delta_captured(changed_delta());
    let requests = driver.role_started(REVIEWER_ROLE);
    let prompt = prompt_text(&requests, REVIEWER_ROLE);
    assert!(
        prompt.contains("This is a corrective pass"),
        "a review after a forward verifies the prior findings"
    );
    let command_id = prompted(&requests, REVIEWER_ROLE);
    let requests = driver.role_turn_completed(&command_id, r#"{"findings":[]}"#);
    assert!(
        requests.contains(&ReviewRequest::ClearPriorReview),
        "a resolved verification pass consumes the prior review: {requests:?}"
    );
}

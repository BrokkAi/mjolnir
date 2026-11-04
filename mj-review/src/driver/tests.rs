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
        base_commit: None,
        head_commit: None,
        base_kind: None,
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
        merged_in: Vec::new(),
        unattributed: None,
        push_review_warning: None,
    }]
}

fn empty_delta() -> Vec<RepoDelta> {
    vec![RepoDelta {
        base_commit: None,
        head_commit: None,
        base_kind: None,
        root: PathBuf::from("/w/app"),
        baseline_tree: Some("base-tree".to_string()),
        current_tree: "base-tree".to_string(),
        patch: String::new(),
        diffstat: "0 files changed".to_string(),
        changed_lines: 0,
        files: Vec::new(),
        merged_in: Vec::new(),
        unattributed: None,
        push_review_warning: None,
    }]
}

/// The command a role was just prompted under, from the requests it
/// produced.
fn prompted(requests: &[ReviewRequest], role: &str) -> String {
    requests
        .iter()
        .find_map(|request| match request {
            ReviewRequest::StartReviewer {
                role: actual,
                command_id,
                ..
            } if actual == role => Some(command_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{role} was not prompted in {requests:?}"))
}

fn prompt_text(requests: &[ReviewRequest], role: &str) -> String {
    requests
        .iter()
        .find_map(|request| match request {
            ReviewRequest::StartReviewer {
                role: actual,
                prompt,
                ..
            } if actual == role => Some(prompt.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{role} was not prompted in {requests:?}"))
}

/// Drives a review to the point where the reviewer has been prompted.
fn running() -> (TurnReviewDriver, String) {
    let (mut driver, requests) = TurnReviewDriver::start(
        seed(),
        "review-test".to_owned(),
        NonZeroUsize::new(1).unwrap(),
    );
    assert_eq!(
        requests,
        vec![ReviewRequest::CaptureDelta {
            baselines: seed().baselines
        }]
    );
    let requests = driver.delta_captured(changed_delta());
    let command_id = prompted(&requests, "reviewer-1");
    let prompt = prompt_text(&requests, "reviewer-1");
    assert!(
        prompt.contains(
            "git -C /w/app diff --no-ext-diff --ignore-submodules=none base-tree new-tree"
        )
    );
    assert!(
        !prompt.contains("+retry"),
        "the prompt does not embed the diff"
    );
    assert!(prompt.contains("add a retry"));
    (driver, command_id)
}

#[test]
fn a_turn_that_changed_nothing_records_a_baseline_and_reviews_nothing() {
    let (mut driver, _) = TurnReviewDriver::start(
        seed(),
        "review-test".to_owned(),
        NonZeroUsize::new(1).unwrap(),
    );
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
    let (mut driver, _) = TurnReviewDriver::start(
        seed,
        "review-test".to_owned(),
        NonZeroUsize::new(1).unwrap(),
    );
    let requests = driver.delta_captured(vec![RepoDelta {
        base_commit: None,
        head_commit: None,
        base_kind: None,
        root: PathBuf::from("/w/app"),
        baseline_tree: None,
        current_tree: "first-tree".to_string(),
        patch: String::new(),
        diffstat: "0 files changed".to_string(),
        changed_lines: 0,
        files: Vec::new(),
        merged_in: Vec::new(),
        unattributed: None,
        push_review_warning: None,
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
    let requests = driver.role_turn_completed(&command_id, "child-1", "No findings.");
    assert_eq!(
        requests,
        vec![
            ReviewRequest::CloseReviewer {
                role: "reviewer-1".to_owned()
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
    let requests = driver.role_turn_completed(
        &command_id,
        "child-1",
        "[P1] src/lib.rs:1 -- unbounded retry loop",
    );
    assert_eq!(
        requests,
        vec![ReviewRequest::CloseReviewer {
            role: "reviewer-1".to_owned()
        }],
        "findings reap the reviewer and start nothing else"
    );
    assert!(driver.can_forward());
    assert!(!driver.finished(), "findings wait to be forwarded");
    assert_eq!(
        driver.last_verdict(),
        Some(&ReviewVerdict::Findings {
            synthesis: "Reviewer 1 (session child-1)\n[P1] src/lib.rs:1 -- unbounded retry loop"
                .to_string(),
            evidence: ReviewPassEvidence::default(),
        })
    );
    assert_eq!(
        driver.roles(),
        vec![RoleStatus {
            role: "reviewer-1".to_string(),
            label: "Reviewer 1".to_string(),
            state: RoleState::Findings,
            child_session_id: None,
        }],
        "a verdict keeps the completed role state available to surfaces"
    );
}

#[test]
fn an_empty_report_fails_the_review_and_leaves_the_baseline_alone() {
    let (mut driver, command_id) = running();
    let requests = driver.role_turn_completed(&command_id, "child-1", "  \n ");
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
    driver.role_turn_completed(&command_id, "child-1", "[P2] src/lib.rs:1 -- weak test");
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
            ReviewRequest::CloseReviewer {
                role: "reviewer-1".to_owned()
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
    driver.role_turn_completed(
        &command_id,
        "child-1",
        "[P1] src/lib.rs:1 -- unbounded retry loop",
    );

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
    driver.role_turn_completed(
        &command_id,
        "child-1",
        "[P1] src/lib.rs:1 -- unbounded retry loop",
    );
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
    driver.role_turn_completed(
        &command_id,
        "child-1",
        "[P1] src/lib.rs:1 -- unbounded retry loop",
    );
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
    driver.role_turn_completed(
        &command_id,
        "child-1",
        "[P1] src/lib.rs:1 -- unbounded retry loop",
    );
    driver.forward("test-forward-command".to_owned());

    assert!(driver.cancel().is_empty());
    assert!(!driver.finished());
}

#[test]
fn a_completion_for_another_command_is_ignored() {
    let (mut driver, _) = running();
    assert!(
        driver
            .role_turn_completed("turn-review-reviewer-999", "child-1", "No findings.")
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
        vec![ReviewRequest::CloseReviewer {
            role: "reviewer-1".to_owned()
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
    let (mut driver, _) = TurnReviewDriver::start(
        seed,
        "review-test".to_owned(),
        NonZeroUsize::new(1).unwrap(),
    );
    let requests = driver.delta_captured(changed_delta());
    let prompt = prompt_text(&requests, "reviewer-1");
    assert!(
        prompt.contains("This is a corrective pass"),
        "a review after a forward verifies the prior findings"
    );
    let command_id = prompted(&requests, "reviewer-1");
    let requests = driver.role_turn_completed(&command_id, "child-1", "No findings.");
    assert!(
        requests.contains(&ReviewRequest::ClearPriorReview),
        "a resolved verification pass consumes the prior review: {requests:?}"
    );
}

#[test]
fn out_of_order_reports_remain_in_launch_order_and_partial_findings_keep_coverage_open() {
    let mut review_seed = seed();
    review_seed.prior_review = Some(PriorReviewContext {
        synthesis: "earlier issue".into(),
        evidence: ReviewPassEvidence::default(),
    });
    let (mut driver, _) =
        TurnReviewDriver::start(review_seed, "group-1".into(), NonZeroUsize::new(3).unwrap());
    let requests = driver.delta_captured(changed_delta());
    let first = prompted(&requests, "reviewer-1");
    let third = prompted(&requests, "reviewer-3");
    assert!(
        driver
            .role_turn_completed(&third, "session-three", "third finding")
            .is_empty()
    );
    assert!(
        driver
            .role_failed("reviewer-2", Some("session-two"), "harness exited")
            .is_empty()
    );
    let stored = serde_json::to_string(&driver.snapshot()).unwrap();
    let mut driver = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert!(
        driver
            .role_turn_completed(&third, "session-three", "duplicate")
            .is_empty()
    );
    let requests = driver.role_turn_completed(&first, "session-one", "first finding");
    assert_eq!(requests.len(), 3);
    let ReviewVerdict::Findings {
        synthesis,
        evidence,
    } = driver.last_verdict().unwrap()
    else {
        panic!("reports must retain findings");
    };
    assert!(synthesis.find("session-one").unwrap() < synthesis.find("session-two").unwrap());
    assert!(synthesis.find("session-two").unwrap() < synthesis.find("session-three").unwrap());
    assert!(synthesis.contains("first finding") && synthesis.contains("third finding"));
    assert_eq!(evidence.reviewer_failures.len(), 1);
    assert!(!driver.last_verdict().unwrap().is_complete());
    let mut dismissed = driver.clone();
    assert_eq!(dismissed.dismiss(), vec![ReviewRequest::Close]);
    driver.forward("forward-partial".into());
    let pending = driver.pending_forward().unwrap();
    let (mut resumed, _) = TurnReviewDriver::resume_forward(seed(), pending);
    resumed.forward("ignored".into());
    assert_eq!(
        resumed.forward_succeeded(),
        vec![ReviewRequest::ClearPendingForward, ReviewRequest::Close]
    );
}

#[test]
fn every_clean_slot_is_required_before_the_group_advances() {
    let (mut driver, _) =
        TurnReviewDriver::start(seed(), "group-clean".into(), NonZeroUsize::new(2).unwrap());
    let requests = driver.delta_captured(changed_delta());
    let first = prompted(&requests, "reviewer-1");
    let second = prompted(&requests, "reviewer-2");
    assert!(
        driver
            .role_turn_completed(&second, "session-two", "No findings.")
            .is_empty()
    );
    assert!(driver.verdict().is_none());
    let requests = driver.role_turn_completed(&first, "session-one", "No findings.");
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. }))
    );
    assert_eq!(driver.last_verdict(), Some(&ReviewVerdict::Clean));
}

#[test]
fn aggregate_keeps_later_reports_after_more_than_sixty_four_kibibytes() {
    let (mut driver, _) =
        TurnReviewDriver::start(seed(), "large-group".into(), NonZeroUsize::new(3).unwrap());
    let requests = driver.delta_captured(changed_delta());
    for index in 1..=3 {
        let command = prompted(&requests, &format!("reviewer-{index}"));
        let report = format!("report-{index}: {}", "x".repeat(31_000));
        driver.role_turn_completed(&command, &format!("session-{index}"), &report);
    }
    let ReviewVerdict::Findings { synthesis, .. } = driver.last_verdict().unwrap() else {
        panic!("expected aggregate findings");
    };
    assert!(synthesis.len() > 64 * 1024);
    for index in 1..=3 {
        assert!(synthesis.contains(&format!("report-{index}:")));
        assert!(synthesis.contains(&format!("session-{index}")));
    }
}

#[test]
fn restart_replays_original_prompt_only_for_unsettled_roles() {
    let (mut driver, capture) =
        TurnReviewDriver::start(seed(), "replay-group".into(), NonZeroUsize::new(2).unwrap());
    assert_eq!(driver.recovery_requests(), capture);
    let starts = driver.delta_captured(changed_delta());
    let first = prompted(&starts, "reviewer-1");
    assert!(
        driver
            .role_turn_completed(&first, "session-one", "No findings.")
            .is_empty()
    );
    let stored = serde_json::to_string(&driver.snapshot()).unwrap();
    let mut restored = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert_eq!(restored.recovery_requests(), vec![starts[1].clone()]);
    assert!(
        restored
            .role_turn_completed(&first, "session-one", "late duplicate")
            .is_empty()
    );
    let second = prompted(&starts, "reviewer-2");
    let settled = restored.role_turn_completed(&second, "session-two", "No findings.");
    let stored = serde_json::to_string(&restored.snapshot()).unwrap();
    let mut restored = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert_eq!(restored.recovery_requests(), settled);
    assert!(
        settled
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. }))
    );
}

#[test]
fn accepted_partial_handoff_replays_its_clear_and_close_after_restart() {
    let (mut driver, _) = TurnReviewDriver::start(
        seed(),
        "partial-replay".into(),
        NonZeroUsize::new(2).unwrap(),
    );
    let starts = driver.delta_captured(changed_delta());
    driver.role_turn_completed(&prompted(&starts, "reviewer-1"), "session-one", "a finding");
    driver.role_failed("reviewer-2", Some("session-two"), "harness exited");
    let sent = driver.forward("stable-forward-id".into());
    let stored = serde_json::to_string(&driver.snapshot()).unwrap();
    let mut restored = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert_eq!(restored.recovery_requests(), sent);
    let final_work = restored.forward_succeeded();
    assert_eq!(
        final_work,
        vec![ReviewRequest::ClearPendingForward, ReviewRequest::Close]
    );
    let stored = serde_json::to_string(&restored.snapshot()).unwrap();
    let mut restored = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert_eq!(restored.recovery_requests(), final_work);
}

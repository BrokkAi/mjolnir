use super::*;
use crate::verdict::ReviewPassEvidence;
use mj_core::review::lanes::UserMessage;
use std::path::PathBuf;

fn seed() -> TurnReviewSeed {
    TurnReviewSeed {
        task: "add a retry".to_string(),
        user_messages: vec![UserMessage::prompt("add a retry")],
        declined: Vec::new(),
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
    assert_eq!(requests, vec![ReviewRequest::CaptureDelta]);
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

// Hard-won: 2a11e18a80: a workspace without a baseline was misleadingly labeled unchanged.

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
    assert!(!driver.finished(), "findings await mailbox delivery");
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
    driver.role_turn_completed(&command_id, "child-1", "  \n ");
    let ReviewVerdict::Failed { reason } = driver
        .last_verdict()
        .expect("the failed verdict is retained")
    else {
        panic!(
            "an empty report must fail the review, got {:?}",
            driver.phase()
        );
    };
    assert!(reason.contains("empty report"), "{reason}");
}

/// The primary is told what it is getting: one reviewer's findings that
/// nobody checked, which it verifies against source before acting.

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
    assert_eq!(
        driver.phase(),
        &TurnReviewPhase::Resolved(Resolution::Cancelled)
    );
    assert!(driver.cancel().is_empty(), "cancelling twice is inert");
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
fn a_request_failure_closes_without_advancing_coverage() {
    let (mut driver, _) = running();
    let requests = driver.request_failed("the reviewer could not start");
    assert_eq!(
        requests,
        vec![ReviewRequest::CloseReviewer {
            role: "reviewer-1".to_owned()
        }]
    );
    assert!(matches!(
        driver.last_verdict(),
        Some(ReviewVerdict::Failed { .. })
    ));
    assert!(!driver.finished(), "failure awaits mailbox delivery");
    let stored = serde_json::to_string(&driver.snapshot()).unwrap();
    let mut restored = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert_eq!(restored.last_verdict(), driver.last_verdict());
    assert_eq!(restored.recovery_requests(), requests);
}

#[test]
fn an_empty_push_snapshot_can_resume_verdict_delivery() {
    let (mut driver, _) = TurnReviewDriver::start(seed(), "empty-push".into(), NonZeroUsize::MIN);
    let mut deltas = changed_delta();
    deltas[0].current_tree = deltas[0].baseline_tree.clone().unwrap();
    deltas[0].patch.clear();
    deltas[0].files.clear();
    deltas[0].changed_lines = 0;
    driver.empty_push_delta_captured(deltas);
    let stored = serde_json::to_string(&driver.snapshot()).unwrap();
    let restored = TurnReviewDriver::restore(serde_json::from_str(&stored).unwrap()).unwrap();
    assert_eq!(restored.last_verdict(), Some(&ReviewVerdict::Clean));
    assert!(restored.awaited_commands().is_empty());
}

#[test]
fn the_reviewer_prompt_carries_the_seed_declined_findings_and_no_corrective_pass() {
    let mut seed = seed();
    seed.declined = vec![mj_core::subagent::ReviewOverride {
        finding: "[P1] src/lib.rs:1 -- no bound".to_string(),
        reason: "The caller bounds it".to_string(),
    }];
    let (mut driver, _) = TurnReviewDriver::start(
        seed,
        "review-test".to_owned(),
        NonZeroUsize::new(1).unwrap(),
    );
    let requests = driver.delta_captured(changed_delta());
    let prompt = prompt_text(&requests, "reviewer-1");
    assert!(!prompt.contains("corrective pass"));
    assert!(prompt.contains("Finding: [P1] src/lib.rs:1 -- no bound"));
    assert!(prompt.contains("Reason: The caller bounds it"));
}

#[test]
fn out_of_order_reports_remain_in_launch_order_and_partial_findings_keep_coverage_open() {
    let review_seed = seed();
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
    driver.role_turn_completed(&first, "session-one", "No findings.");
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
    assert_eq!(restored.last_verdict(), Some(&ReviewVerdict::Clean));
}

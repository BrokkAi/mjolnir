use super::*;
use crate::verdict::ReviewPassEvidence;
use mj_core::review::lanes::UserMessage;

fn seed() -> TurnReviewSeed {
    TurnReviewSeed {
        tier: ReviewTier::Quick,
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

/// Drives a quick review to the point where the reviewer has been prompted.
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
        "the quick tier starts its one reviewer and runs no change analysis"
    );
    let requests = driver.role_started(REVIEWER_ROLE);
    let command_id = prompted(&requests, REVIEWER_ROLE);
    let prompt = prompt_text(&requests, REVIEWER_ROLE);
    assert!(prompt.contains("+retry"), "the prompt carries the capture");
    assert!(prompt.contains("add a retry"));
    (driver, command_id)
}

/// Drives an extended review to the point where the supervisor is working.
fn supervising() -> (TurnReviewDriver, String) {
    let mut seed = seed();
    seed.tier = ReviewTier::Extended;
    // Two governing messages: the supervisor reads both itself.
    seed.user_messages
        .push(UserMessage::prompt("bound the retry"));
    let (mut driver, _) = TurnReviewDriver::start(seed);
    let requests = driver.delta_captured(changed_delta());
    assert_eq!(
        requests,
        vec![ReviewRequest::StartRole {
            role: SUPERVISOR_ROLE.to_string(),
            fresh: true
        }],
        "the supervisor starts straight after the capture: no intent analyst \
         and no change analysis runs ahead of it"
    );
    let requests = driver.role_started(SUPERVISOR_ROLE);
    let prompt = prompt_text(&requests, SUPERVISOR_ROLE);
    assert!(
        prompt.contains("bound the retry"),
        "the supervisor reads the user's messages"
    );
    assert!(prompt.contains(crate::lanes::INTENT_CONTEXT));
    assert!(
        prompt.contains("<changed_files") && prompt.contains("src/lib.rs"),
        "the supervisor reads Git's per-file line counts"
    );
    assert!(!prompt.contains("changed_functions"));
    assert!(prompt.contains("spawn_specialist"));
    let command_id = prompted(&requests, SUPERVISOR_ROLE);
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
fn an_empty_quick_report_fails_the_review_and_leaves_the_baseline_alone() {
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
            .role_turn_completed("turn-review-reviewer-999", "No findings.")
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
        prompt.contains("This is a verification pass"),
        "a review after a forward verifies the prior findings"
    );
    let command_id = prompted(&requests, REVIEWER_ROLE);
    let requests = driver.role_turn_completed(&command_id, "No findings.");
    assert!(
        requests.contains(&ReviewRequest::ClearPriorReview),
        "a resolved verification pass consumes the prior review: {requests:?}"
    );
}

#[test]
fn the_supervisor_launches_the_lanes_it_asks_for_and_waits_for_each() {
    let (mut driver, supervisor) = supervising();
    let requests = driver.lanes_dispatched(vec![
        ReviewSubagentRequest {
            agent_type: "tests".to_string(),
            hypothesis: "the new test cannot fail for the reason it claims".to_string(),
        },
        ReviewSubagentRequest {
            agent_type: "error_handling".to_string(),
            hypothesis: "the retry may swallow cancellation".to_string(),
        },
    ]);
    assert_eq!(
        requests,
        vec![
            ReviewRequest::StartRole {
                role: "tests".to_string(),
                fresh: true
            },
            ReviewRequest::StartRole {
                role: "error_handling".to_string(),
                fresh: true
            },
        ]
    );
    // A lane already launched is not launched again.
    assert!(
        driver
            .lanes_dispatched(vec![ReviewSubagentRequest {
                agent_type: "tests".to_string(),
                hypothesis: "the same lane again".to_string(),
            }])
            .is_empty()
    );

    let tests = prompted(&driver.role_started("tests"), "tests");
    let error_handling = prompted(&driver.role_started("error_handling"), "error_handling");

    // The supervisor ends its turn while both lanes are still running: it
    // may not conclude, and nothing is injected until a report exists.
    assert!(
        driver
            .role_turn_completed(&supervisor, "Waiting on the specialists.")
            .is_empty()
    );
    assert!(driver.verdict().is_none(), "a verdict is blocked");

    // Reports arrive out of order; each is injected as it lands.
    let requests = driver.role_turn_completed(&error_handling, "[P1] src/lib.rs:3 -- swallowed");
    let injection = prompt_text(&requests, SUPERVISOR_ROLE);
    assert!(injection.contains("lane=\"error_handling\""));
    assert!(
        injection.contains("do not issue the final verdict yet"),
        "one lane is still outstanding"
    );
    let supervisor = prompted(&requests, SUPERVISOR_ROLE);
    assert!(requests.contains(&ReviewRequest::PauseRole {
        role: "error_handling".to_string()
    }));

    // The supervisor ends that turn before the last lane reports.
    assert!(
        driver
            .role_turn_completed(&supervisor, "Still waiting.")
            .is_empty()
    );
    let requests = driver.role_turn_completed(&tests, "No findings.");
    let injection = prompt_text(&requests, SUPERVISOR_ROLE);
    assert!(injection.contains("All currently selected reviewers have now reported"));
    let supervisor = prompted(&requests, SUPERVISOR_ROLE);

    let requests = driver.role_turn_completed(&supervisor, "[P1] src/lib.rs:3 -- swallowed");
    assert!(driver.can_forward(), "the synthesis is the verdict");
    assert!(
        requests
            .iter()
            .all(|request| matches!(request, ReviewRequest::PauseRole { .. })),
        "every role is reaped before the verdict waits for the user: {requests:?}"
    );
    let ReviewVerdict::Findings { evidence, .. } = driver.verdict().unwrap() else {
        panic!("a findings verdict carries its lane coverage");
    };
    assert_eq!(evidence.lanes.len(), 2);
}

#[test]
fn a_lane_that_cannot_start_reaches_the_supervisor_as_a_coverage_gap() {
    let (mut driver, supervisor) = supervising();
    driver.lanes_dispatched(vec![ReviewSubagentRequest {
        agent_type: "dead_code".to_string(),
        hypothesis: "the new helper may be unused".to_string(),
    }]);
    assert!(
        driver
            .role_turn_completed(&supervisor, "Waiting.")
            .is_empty()
    );
    let requests = driver.lane_failed("dead_code", "the harness could not start");
    let injection = prompt_text(&requests, SUPERVISOR_ROLE);
    assert!(injection.contains("outcome=\"failed: the harness could not start\""));
    assert!(injection.contains("All currently selected reviewers have now reported"));
}

#[test]
fn a_dispatch_of_an_unknown_or_duplicate_lane_is_refused() {
    let (mut driver, _) = supervising();
    assert!(
        driver
            .lanes_dispatched(vec![ReviewSubagentRequest {
                agent_type: "quick".to_string(),
                hypothesis: "the quick reviewer is not a lane".to_string(),
            }])
            .is_empty()
    );
    assert!(
        driver
            .lanes_dispatched(vec![
                ReviewSubagentRequest {
                    agent_type: "tests".to_string(),
                    hypothesis: "first".to_string(),
                },
                ReviewSubagentRequest {
                    agent_type: "tests".to_string(),
                    hypothesis: "second".to_string(),
                },
            ])
            .is_empty(),
        "a dispatch that names one lane twice is refused whole"
    );
}

#[test]
fn cancelling_mid_fanout_reaps_every_role_and_keeps_the_baseline() {
    let (mut driver, _) = supervising();
    driver.lanes_dispatched(vec![ReviewSubagentRequest {
        agent_type: "duplication".to_string(),
        hypothesis: "the helper may already exist".to_string(),
    }]);
    driver.role_started("duplication");
    let requests = driver.cancel();
    let paused = requests
        .iter()
        .filter_map(|request| match request {
            ReviewRequest::PauseRole { role } => Some(role.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        paused,
        BTreeSet::from([SUPERVISOR_ROLE.to_string(), "duplication".to_string()]),
        "every started role is reaped"
    );
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. }))
    );
}

/// A supervisor's synthesis keeps the wording it always had, which is also what
/// a handoff persisted before provenance was recorded retries with.
#[test]
fn an_extended_forward_keeps_the_vetted_wording() {
    let (mut driver, command_id) = supervising();
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    assert!(driver.can_forward(), "{:?}", driver.phase());
    let requests = driver.forward("test-forward-command".to_owned());
    let [ReviewRequest::PromptPrimary { prompt, .. }] = requests.as_slice() else {
        panic!("forward submits one primary prompt, got {requests:?}");
    };
    assert_eq!(
        prompt,
        &correction_note(
            "[P1] src/lib.rs:1 -- unbounded retry loop",
            FindingsProvenance::Vetted
        )
    );
    assert!(prompt.contains("validated by a reviewing agent"));
    let pending = driver
        .pending_forward()
        .expect("forwarding state is durable");
    assert_eq!(pending.provenance, FindingsProvenance::Vetted);
    let stored = serde_json::to_value(&pending).unwrap();
    assert!(
        stored.get("provenance").is_none(),
        "a vetted handoff is stored exactly as before provenance existed: {stored}"
    );
}

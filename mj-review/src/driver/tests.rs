
use super::*;
use crate::verdict::ReviewPassEvidence;
use mj_core::review::lanes::UserMessage;

fn seed() -> TurnReviewSeed {
    TurnReviewSeed {
        tier: ReviewTier::Quick,
        task: "add a retry".to_string(),
        user_messages: vec![UserMessage::prompt("add a retry")],
        initial_result: "added a retry".to_string(),
        trajectory: "edited src/lib.rs".to_string(),
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
    assert!(matches!(
        requests.as_slice(),
        [
            ReviewRequest::AnalyzeDelta { .. },
            ReviewRequest::StartRole { .. }
        ]
    ));
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
    // Two governing messages, so the intent analyst is worth running.
    seed.user_messages
        .push(UserMessage::prompt("bound the retry"));
    let (mut driver, _) = TurnReviewDriver::start(seed);
    let requests = driver.delta_captured(changed_delta());
    assert!(
        requests.contains(&ReviewRequest::StartRole {
            role: INTENT_ROLE.to_string(),
            fresh: true
        }),
        "a turn with several governing messages runs the intent analyst: {requests:?}"
    );
    let requests = driver.role_started(INTENT_ROLE);
    let intent_command = prompted(&requests, INTENT_ROLE);
    // The analysis lands while the analyst is still working, which is the
    // concurrency mj's own shape has.
    assert!(
        driver
            .analysis_completed(Ok("- edited retry()".to_string()))
            .is_empty(),
        "the supervisor waits for the intent brief its prompt embeds"
    );
    let requests = driver.role_turn_completed(&intent_command, "Goal: bound the retry");
    assert!(
        requests.contains(&ReviewRequest::StartRole {
            role: SUPERVISOR_ROLE.to_string(),
            fresh: true
        }),
        "the supervisor starts once both inputs exist: {requests:?}"
    );
    let requests = driver.role_started(SUPERVISOR_ROLE);
    let prompt = prompt_text(&requests, SUPERVISOR_ROLE);
    assert!(prompt.contains("Goal: bound the retry"));
    assert!(prompt.contains("- edited retry()"));
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
fn a_clean_review_spends_no_validator_and_advances_the_baseline_itself() {
    let (mut driver, command_id) = running();
    let requests = driver.role_turn_completed(&command_id, "No findings.");
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
        "a clean reviewer releases the turn without a validator"
    );
    assert!(driver.finished());
    assert_eq!(
        driver.last_verdict(),
        Some(&ReviewVerdict::Clean),
        "the clean verdict remains available for the close notice"
    );
}

#[test]
fn findings_reach_a_validator_only_once_the_analysis_is_ready() {
    let (mut driver, command_id) = running();
    let requests = driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    assert!(
        requests.is_empty(),
        "nothing starts while the analysis is still running: {requests:?}"
    );
    let requests = driver.analysis_completed(Ok("- edited retry()".to_string()));
    assert_eq!(
        requests,
        vec![
            ReviewRequest::PauseRole {
                role: REVIEWER_ROLE.to_string()
            },
            ReviewRequest::StartRole {
                role: VALIDATOR_ROLE.to_string(),
                fresh: true
            }
        ],
        "the reviewer is reaped before the validator is staged over it"
    );
    let requests = driver.role_started(VALIDATOR_ROLE);
    let prompt = prompt_text(&requests, VALIDATOR_ROLE);
    assert!(prompt.contains("[P1] src/lib.rs:1 -- no bound"));
    assert!(prompt.contains("- edited retry()"));
    let command_id = prompted(&requests, VALIDATOR_ROLE);

    let requests =
        driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    assert!(
        requests
            .iter()
            .all(|request| matches!(request, ReviewRequest::PauseRole { .. })),
        "a findings verdict reaps its roles and waits for the user: {requests:?}"
    );
    assert!(driver.can_forward());
    assert!(!driver.finished(), "findings wait for the user");
    assert!(matches!(
        driver.last_verdict(),
        Some(ReviewVerdict::Findings { .. })
    ));
    assert_eq!(
        driver.roles(),
        vec![
            RoleStatus {
                role: REVIEWER_ROLE.to_string(),
                label: super::super::lanes::QUICK_LANE.label.to_string(),
                state: RoleState::Findings,
            },
            RoleStatus {
                role: VALIDATOR_ROLE.to_string(),
                label: "Validator".to_string(),
                state: RoleState::Clean,
            },
        ],
        "a verdict keeps the completed role states available to surfaces"
    );
}

#[test]
fn an_analysis_that_lands_before_the_findings_starts_the_validator_at_once() {
    let (mut driver, command_id) = running();
    assert!(
        driver
            .analysis_completed(Ok("- edited retry()".to_string()))
            .is_empty(),
        "a clean review must never wait on the analysis"
    );
    let requests = driver.role_turn_completed(&command_id, "[P2] src/lib.rs:1 -- weak test");
    assert_eq!(
        requests,
        vec![
            ReviewRequest::PauseRole {
                role: REVIEWER_ROLE.to_string()
            },
            ReviewRequest::StartRole {
                role: VALIDATOR_ROLE.to_string(),
                fresh: true
            }
        ]
    );
}

#[test]
fn a_failed_analysis_fails_the_review_and_leaves_the_baseline_alone() {
    let (mut driver, command_id) = running();
    assert!(
        driver
            .analysis_completed(Err("bifrost exited with 1".to_string()))
            .is_empty()
    );
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    let ReviewVerdict::Failed { reason } = driver.verdict().expect("a verdict is on screen") else {
        panic!(
            "a failed analysis must fail the review, got {:?}",
            driver.phase()
        );
    };
    assert!(reason.contains("bifrost exited with 1"));
    assert!(matches!(
        driver.last_verdict(),
        Some(ReviewVerdict::Failed { .. })
    ));
    assert!(!driver.can_forward());
    let requests = driver.dismiss();
    assert_eq!(
        requests,
        vec![ReviewRequest::Close],
        "a failed review never advances the baseline"
    );
    assert!(driver.finished());
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
fn forwarding_sends_the_synthesis_and_makes_the_next_review_a_verification_pass() {
    let (mut driver, command_id) = running();
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    let requests = driver.role_started(VALIDATOR_ROLE);
    let command_id = prompted(&requests, VALIDATOR_ROLE);
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");

    let requests = driver.forward("test-forward-command".to_owned());
    let [ReviewRequest::PromptPrimary { command_id, prompt }] = requests.as_slice() else {
        panic!("forwarding first waits for primary acceptance, got {requests:?}");
    };
    assert!(prompt.contains("[P1] src/lib.rs:1 -- unbounded retry loop"));
    assert!(prompt.contains("HARNESS NOTE"));
    assert_eq!(
        driver.phase(),
        &TurnReviewPhase::Forwarding {
            synthesis: "[P1] src/lib.rs:1 -- unbounded retry loop".to_string(),
            evidence: ReviewPassEvidence::default(),
            command_id: command_id.clone(),
            error: None,
        }
    );
    assert!(
        !driver.finished(),
        "forwarding waits for primary acceptance"
    );

    let requests = driver.forward_succeeded();
    let [
        ReviewRequest::RecordPriorReview { prior },
        ReviewRequest::AdvanceBaseline { trees, .. },
        ReviewRequest::Close,
    ] = requests.as_slice()
    else {
        panic!("accepted forwarding records coverage and closes, got {requests:?}");
    };
    assert!(prior.synthesis.contains("unbounded retry loop"));
    assert_eq!(trees[&PathBuf::from("/w/app")], "new-tree");
    assert!(driver.finished());
}

#[test]
fn a_rejected_forward_keeps_findings_and_retries_the_same_command() {
    let (mut driver, command_id) = running();
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    let requests = driver.role_started(VALIDATOR_ROLE);
    let command_id = prompted(&requests, VALIDATOR_ROLE);
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
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    let requests = driver.role_started(VALIDATOR_ROLE);
    let command_id = prompted(&requests, VALIDATOR_ROLE);
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
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    let requests = driver.role_started(VALIDATOR_ROLE);
    let command_id = prompted(&requests, VALIDATOR_ROLE);
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    driver.forward("test-forward-command".to_owned());
    let pending = driver
        .pending_forward()
        .expect("forwarding state is durable");

    let (mut resumed, initial) = TurnReviewDriver::resume_forward(seed(), pending.clone());
    assert!(
        initial.is_empty(),
        "recovery starts at the handoff boundary"
    );
    assert_eq!(
        resumed.forward("unused-new-command".to_owned()),
        vec![ReviewRequest::PromptPrimary {
            command_id: pending.command_id,
            prompt: correction_note(&pending.synthesis),
        }]
    );
}

#[test]
fn a_pending_forward_cannot_be_cancelled_before_the_relay_acknowledges_it() {
    let (mut driver, command_id) = running();
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- no bound");
    let requests = driver.role_started(VALIDATOR_ROLE);
    let command_id = prompted(&requests, VALIDATOR_ROLE);
    driver.role_turn_completed(&command_id, "[P1] src/lib.rs:1 -- unbounded retry loop");
    driver.forward("test-forward-command".to_owned());

    assert!(driver.cancel().is_empty());
    assert!(!driver.finished());
}

#[test]
fn dismissing_findings_advances_the_baseline_without_prompting_the_primary() {
    let (mut driver, command_id) = running();
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    driver.role_turn_completed(&command_id, "[P3] src/lib.rs:1 -- nit");
    let requests = driver.role_started(VALIDATOR_ROLE);
    let command_id = prompted(&requests, VALIDATOR_ROLE);
    driver.role_turn_completed(&command_id, "[P3] src/lib.rs:1 -- nit");

    let requests = driver.dismiss();
    assert_eq!(
        requests,
        vec![
            ReviewRequest::AdvanceBaseline {
                trees: BTreeMap::from([(PathBuf::from("/w/app"), "new-tree".to_string())]),
                reviewed_through_ordinal: 12,
            },
            ReviewRequest::Close,
        ]
    );
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
fn one_governing_message_skips_the_intent_analyst() {
    let mut seed = seed();
    seed.tier = ReviewTier::Extended;
    let (mut driver, _) = TurnReviewDriver::start(seed);
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    let requests = driver.delta_captured(changed_delta());
    assert!(
        requests.contains(&ReviewRequest::StartRole {
            role: SUPERVISOR_ROLE.to_string(),
            fresh: true
        }),
        "a self-contained prompt reaches the supervisor verbatim: {requests:?}"
    );
    assert!(
        !requests.contains(&ReviewRequest::StartRole {
            role: INTENT_ROLE.to_string(),
            fresh: true
        }),
        "no analyst runs when there is nothing to reconcile"
    );
    let prompt = prompt_text(&driver.role_started(SUPERVISOR_ROLE), SUPERVISOR_ROLE);
    assert!(prompt.contains(DIRECT_INTENT_CONTEXT));
}

#[test]
fn an_empty_intent_brief_fails_the_review_rather_than_proceeding_without_one() {
    let mut seed = seed();
    seed.tier = ReviewTier::Extended;
    seed.user_messages.push(UserMessage::prompt("bound it"));
    let (mut driver, _) = TurnReviewDriver::start(seed);
    driver.delta_captured(changed_delta());
    driver.analysis_completed(Ok("- edited retry()".to_string()));
    let requests = driver.role_started(INTENT_ROLE);
    let command_id = prompted(&requests, INTENT_ROLE);
    driver.role_turn_completed(&command_id, "   ");
    assert!(matches!(
        driver.verdict(),
        Some(ReviewVerdict::Failed { .. })
    ));
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
    assert!(evidence.intent_available);
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
        BTreeSet::from([
            INTENT_ROLE.to_string(),
            SUPERVISOR_ROLE.to_string(),
            "duplication".to_string(),
        ]),
        "every started role is reaped"
    );
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, ReviewRequest::AdvanceBaseline { .. }))
    );
}

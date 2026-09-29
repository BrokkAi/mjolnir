use super::{test_support::*, *};
use agent_client_protocol::schema::v1::{ContentChunk, TextContent};
use mj_core::config::HarnessKind;

fn open(root: &std::path::Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "test").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
    relay.set_turn_verdict_harness(HarnessKind::Codex);
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    relay
}

fn start(relay: &mut DurableRelay, id: &str) {
    submit_relay(relay, id, prompt("work"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        id
    );
}

fn finish(relay: &mut DurableRelay, id: &str, text: &str, stop: &str) {
    relay
        .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )))
        .unwrap();
    relay
        .record_command_completed(
            id,
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: stop.into(),
                usage: None,
            },
        )
        .unwrap();
}

fn assess(relay: &mut DurableRelay, retryable: bool) {
    let identity = relay.retry_assessment_identity();
    assert!(identity.is_some());
    assert_eq!(
        relay.resolve_retry_assessment(identity, retryable).unwrap(),
        retryable
    );
}

#[test]
fn server_retry_requires_a_jev_verdict_and_retries_with_backoff() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    let mut id = "original-prompt".to_owned();
    for (index, minutes) in [1, 2, 4, 8, 16, 16].into_iter().enumerate() {
        finish(&mut relay, &id, "Provider temporarily unavailable", "error");
        assert!(relay.capacity_retry_deadline().is_none());
        assert!(relay.operational_state().retry_assessment_pending);
        assess(&mut relay, true);
        let retry = relay.operational_state().capacity_retry.unwrap();
        assert_eq!(retry.attempt, index as u32 + 1);
        assert_eq!(
            retry.retry_at_ms - relay.hot_events.back().unwrap().recorded_at_ms,
            minutes * 60_000
        );
        assert!(
            !relay
                .submit_due_capacity_retry(retry.retry_at_ms - 1)
                .unwrap()
        );
        assert!(relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
        assert!(!relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
        assert_eq!(
            relay.snapshot.continuation.user_command_id.as_deref(),
            Some("original-prompt"),
            "an automatic retry must not become new user authorization"
        );
        let commands = relay.claim_pending_commands(true).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, prompt("Continue"));
        id = commands[0].command_id.clone();
    }
    finish(&mut relay, &id, "Done", "EndTurn");
    assess(&mut relay, false);
    assert!(relay.capacity_retry_deadline().is_none());
    assert!(relay.snapshot.capacity_retry.is_none());
    // A later autonomous refusal starts a new backoff sequence.
    relay
        .record_observation(RelayObservation::HarnessTurnStarted {
            started_at_ms: epoch_millis(),
        })
        .unwrap();
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    assess(&mut relay, true);
    assert_eq!(relay.operational_state().capacity_retry.unwrap().attempt, 1);
}

#[test]
fn retry_dispatches_for_each_harness_after_jev_approval() {
    let configurations = [
        (HarnessKind::Codex, BackgroundWorkPolicy::CodexExecCards),
        (HarnessKind::Claude, BackgroundWorkPolicy::ClaudeTasks),
        (HarnessKind::Kimi, BackgroundWorkPolicy::KimiTasks),
        (HarnessKind::Grok, BackgroundWorkPolicy::HostedTerminals),
        (HarnessKind::Muse, BackgroundWorkPolicy::HostedTerminals),
    ];
    for (harness, policy) in configurations {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        relay.set_turn_verdict_harness(harness);
        relay.set_background_work_policy(policy);
        if harness == HarnessKind::Kimi {
            relay
                .kimi_background_tasks_changed(
                    Vec::new(),
                    std::collections::BTreeSet::new(),
                    std::collections::BTreeSet::new(),
                )
                .unwrap();
        }
        start(&mut relay, "original-prompt");
        finish(
            &mut relay,
            "original-prompt",
            "Service temporarily unavailable",
            "error",
        );
        relay.prepare_pending_assessment().unwrap();
        assert_eq!(
            relay
                .snapshot
                .assessment
                .as_ref()
                .unwrap()
                .evidence
                .as_ref()
                .unwrap()
                .harness,
            harness
        );
        assess(&mut relay, true);
        let due = relay.capacity_retry_deadline().unwrap();
        assert!(relay.submit_due_capacity_retry(due).unwrap(), "{harness:?}");
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command,
            prompt("Continue"),
            "{harness:?}"
        );
    }
}

#[test]
fn text_only_capacity_message_and_structured_errors_wait_for_jev() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "text-prompt");
    finish(
        &mut relay,
        "text-prompt",
        "Selected model is at capacity. Please try a different model.",
        "EndTurn",
    );
    assert!(relay.capacity_retry_deadline().is_none());
    relay.prepare_pending_assessment().unwrap();
    let (_, evidence, identity) = relay.pending_replied_verdict().unwrap();
    assert!(identity.is_some());
    assert_eq!(evidence.completion.unwrap().stop_reason, "EndTurn");
    assess(&mut relay, false);
    assert!(relay.capacity_retry_deadline().is_none());

    start(&mut relay, "legacy-reason-prompt");
    finish(
        &mut relay,
        "legacy-reason-prompt",
        "Provider unavailable",
        "ModelCapacity",
    );
    assert!(relay.capacity_retry_deadline().is_none());
    assess(&mut relay, false);
    assert!(relay.capacity_retry_deadline().is_none());

    start(&mut relay, "structured-prompt");
    relay
        .record_command_completed(
            "structured-prompt",
            RelayCommandOutcome::Prompt {
                stop_reason: "error".into(),
                diagnostic: Some(mj_core::diagnostic::TurnDiagnostic::from_acp(
                    &agent_client_protocol::Error::new(-32603, "failed")
                        .data(serde_json::json!({"codex_error_info":"server_overloaded"})),
                )),
                usage: None,
            },
        )
        .unwrap();
    relay.prepare_pending_assessment().unwrap();
    let evidence = relay
        .snapshot
        .assessment
        .as_ref()
        .unwrap()
        .evidence
        .as_ref()
        .unwrap();
    assert_eq!(
        evidence
            .completion
            .as_ref()
            .unwrap()
            .diagnostic
            .as_ref()
            .unwrap()
            .code
            .as_deref(),
        Some("server_overloaded")
    );
    assert!(relay.capacity_retry_deadline().is_none());
    assess(&mut relay, true);
    assert!(relay.capacity_retry_deadline().is_some());
}

#[test]
fn pending_assessment_and_armed_retry_survive_worker_reopen() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(
        &mut relay,
        "original-prompt",
        "temporary service failure",
        "error",
    );
    drop(relay);
    let mut relay = open(root.path());
    assert!(relay.operational_state().retry_assessment_pending);
    relay.prepare_pending_assessment().unwrap();
    assert!(relay.pending_replied_verdict().is_some());
    assess(&mut relay, true);
    let retry = relay.operational_state().capacity_retry.unwrap();
    drop(relay);
    let mut relay = open(root.path());
    assert_eq!(
        relay.operational_state().capacity_retry,
        Some(retry.clone())
    );
    assert!(relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
    assert!(!relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
}

#[test]
fn newer_user_work_cancels_pending_assessment() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(
        &mut relay,
        "original-prompt",
        "temporary service failure",
        "error",
    );
    let identity = relay.retry_assessment_identity();
    submit_relay(&mut relay, "new-prompt", prompt("Do something else"));
    assert!(!relay.operational_state().retry_assessment_pending);
    assert!(!relay.resolve_retry_assessment(identity, true).unwrap());
    assert!(relay.capacity_retry_deadline().is_none());
}

#[test]
fn autonomous_provider_failure_arms_one_retry_and_survives_restart() {
    use mj_core::assessment::{Action, Failure, Input, Judgment, Verdict, Work};
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(
        &mut relay,
        "original-prompt",
        "Continuing the requested work.",
        "EndTurn",
    );
    relay
        .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
        .unwrap();
    relay
        .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(
                "Selected model is at capacity. Please try a different model.",
            )),
        )))
        .unwrap();
    relay.settle_harness_turn(Some("test".into())).unwrap();
    relay.prepare_pending_assessment().unwrap();
    let (generation, evidence, _) = relay.pending_replied_verdict().unwrap();
    assert!(evidence.completion.is_some());
    let answer = Verdict {
        failure: Judgment {
            choice: Failure::TransientProvider,
            confidence: 0.91,
        },
        input: Judgment {
            choice: Input::Unclear,
            confidence: 0.32,
        },
        work: Judgment {
            choice: Work::Unclear,
            confidence: 0.24,
        },
    };
    assert_eq!(
        relay.apply_turn_assessment(generation, answer).unwrap(),
        "server_retry_armed"
    );
    let retry = relay.operational_state().capacity_retry.unwrap();
    assert_eq!(
        relay.snapshot.assessment.as_ref().unwrap().action,
        Some(Action::RetryProvider)
    );
    drop(relay);
    let mut relay = open(root.path());
    assert_eq!(relay.capacity_retry_deadline(), Some(retry.retry_at_ms));
    assert!(relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
    assert!(!relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    let id = format!("assessment-{SESSION}-{generation}");
    let page = mj_core::jev::read(&root.path().join("jev-decisions"), SESSION, Some(&id)).unwrap();
    assert_eq!(page.decisions.len(), 1);
    assert_eq!(page.decisions[0].status, "consumed");
    assert_eq!(page.decisions[0].action, "automatic action admitted");
}

#[test]
fn a_new_user_command_supersedes_an_in_flight_autonomous_assessment() {
    use mj_core::assessment::{Failure, Input, Judgment, Verdict, Work};
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(
        &mut relay,
        "original-prompt",
        "Selected model is at capacity.",
        "EndTurn",
    );
    relay.prepare_pending_assessment().unwrap();
    let (generation, _, _) = relay.pending_replied_verdict().unwrap();
    submit_relay(
        &mut relay,
        "replacement-prompt",
        prompt("Stop that task and do this instead"),
    );
    let answer = Verdict {
        failure: Judgment {
            choice: Failure::TransientProvider,
            confidence: 1.0,
        },
        input: Judgment {
            choice: Input::None,
            confidence: 1.0,
        },
        work: Judgment {
            choice: Work::Unclear,
            confidence: 0.5,
        },
    };
    assert_eq!(
        relay.apply_turn_assessment(generation, answer).unwrap(),
        "stale_turn"
    );
    assert!(relay.capacity_retry_deadline().is_none());
}

#[test]
fn uncertain_assessment_is_cached_and_pending_completion_recovers() {
    use mj_core::assessment::{Failure, Input, Judgment, Verdict, Work};
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(
        &mut relay,
        "original-prompt",
        "Insufficient information.",
        "EndTurn",
    );
    drop(relay);
    let mut relay = open(root.path());
    relay.prepare_pending_assessment().unwrap();
    let (generation, evidence, _) = relay.pending_replied_verdict().unwrap();
    assert!(evidence.authorization.unwrap().authorization_complete);
    let answer = Verdict {
        failure: Judgment {
            choice: Failure::Unclear,
            confidence: 0.5,
        },
        input: Judgment {
            choice: Input::Unclear,
            confidence: 0.5,
        },
        work: Judgment {
            choice: Work::Unclear,
            confidence: 0.5,
        },
    };
    relay.apply_turn_assessment(generation, answer).unwrap();
    assert!(relay.pending_replied_verdict().is_none());
    drop(relay);
    let mut relay = open(root.path());
    relay.prepare_pending_assessment().unwrap();
    assert!(relay.pending_replied_verdict().is_none());
}

#[test]
fn classification_transport_backoff_and_diagnostics_survive_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "request-one");
    finish(
        &mut relay,
        "request-one",
        "Selected model is at capacity",
        "EndTurn",
    );
    relay.prepare_pending_assessment().unwrap();
    let (revision, evidence, _) = relay.pending_replied_verdict().unwrap();
    relay
        .fail_turn_assessment(revision, "transport_failed")
        .unwrap();
    let a = relay.snapshot.assessment.clone().unwrap();
    assert!(!a.needs_classification(a.retry_at_ms.unwrap() - 1));
    assert!(a.needs_classification(a.retry_at_ms.unwrap()));
    drop(relay);
    let mut relay = open(root.path());
    assert_eq!(relay.snapshot.assessment.as_ref().unwrap(), &a);
    assert!(relay.pending_replied_verdict().is_none());
    assert_eq!(a.evidence, Some(evidence));
    let id = format!("assessment-{SESSION}-{revision}");
    let page = mj_core::jev::read(&root.path().join("jev-decisions"), SESSION, Some(&id)).unwrap();
    assert_eq!(page.decisions.len(), 1);
    assert_eq!(page.decisions[0].status, "failed");
    assert_eq!(page.decisions[0].action, "transport failed");
}

#[test]
fn a_checkpoint_seed_preserves_pending_assessment_and_full_authorization() {
    let source = tempfile::tempdir().unwrap();
    let mut relay = open(source.path());
    start(&mut relay, "original-request");
    finish(
        &mut relay,
        "original-request",
        "Selected model is at capacity",
        "EndTurn",
    );
    relay.prepare_pending_assessment().unwrap();
    let state = mj_core::assessment::Checkpoint {
        version: 1,
        context: relay.snapshot.assessment_context.clone(),
        assessment: relay.snapshot.assessment.clone(),
        continuation: relay.snapshot.continuation.clone(),
        capacity_retry: relay.snapshot.capacity_retry.clone(),
        turn_completion: relay.snapshot.turn_completion.clone(),
    };
    let destination = tempfile::tempdir().unwrap();
    std::fs::write(
        mj_core::relay::restored_relay_seed_path(destination.path()),
        serde_json::to_vec(&serde_json::json!({
            "event_frontier": relay.snapshot.latest_ordinal,
            "event_frontier_digest": relay.snapshot.latest_digest,
            "assessment_state": state,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut restored = open(destination.path());
    let (_, evidence, _) = restored.pending_replied_verdict().unwrap();
    assert_eq!(evidence, state.assessment.unwrap().evidence.unwrap());
    assert_eq!(restored.snapshot.assessment_context, state.context);
}

/// A routine checkpoint is Mjolnir's housekeeping, not user work, so it keeps
/// a pending retry assessment and, once assessed, the armed retry (F24).
#[test]
fn a_routine_checkpoint_keeps_the_pending_assessment_and_the_armed_retry() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(
        &mut relay,
        "original-prompt",
        "temporary service failure",
        "error",
    );
    let identity = relay.retry_assessment_identity();
    assert!(identity.is_some());
    ready_checkpoint(&mut relay, "routine-checkpoint-1");
    assert!(relay.operational_state().retry_assessment_pending);
    assert_eq!(relay.retry_assessment_identity(), identity);
    submit_relay(
        &mut relay,
        "release-1",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "routine-checkpoint-1".into(),
        },
    );
    assess(&mut relay, true);
    let retry = relay.operational_state().capacity_retry.unwrap();
    ready_checkpoint(&mut relay, "routine-checkpoint-2");
    assert_eq!(relay.operational_state().capacity_retry, Some(retry));
}

/// Older journals hold a `RetryAssessmentStarted` record, the durable form of
/// a turn that ended on a capacity refusal. A routine checkpoint must not
/// drop it (F24).
#[test]
fn a_routine_checkpoint_keeps_a_journaled_retry_assessment() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    start(&mut relay, "original-prompt");
    finish(&mut relay, "original-prompt", "temporary failure", "error");
    let context = mj_transcript::turn_context::TurnContext::default();
    context.reset("Implement");
    let evidence = context.evidence(
        HarnessKind::Codex,
        mj_core::activity::verdict::TurnPhase::Running,
        &Default::default(),
        0,
    );
    relay
        .record_observation(RelayObservation::RetryAssessmentStarted {
            command_id: "original-prompt".into(),
            evidence: Box::new(evidence),
        })
        .unwrap();
    let pending = relay.snapshot.retry_assessment.clone();
    assert!(pending.is_some());
    ready_checkpoint(&mut relay, "routine-checkpoint");
    assert_eq!(relay.snapshot.retry_assessment, pending);
    // A user prompt is still real work and still supersedes it.
    submit_relay(
        &mut relay,
        "release-routine-checkpoint",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "routine-checkpoint".into(),
        },
    );
    submit_relay(&mut relay, "new-prompt", prompt("Something else"));
    assert!(relay.snapshot.retry_assessment.is_none());
}

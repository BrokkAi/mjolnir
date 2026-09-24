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
        assert_eq!(
            relay
                .snapshot
                .retry_assessment
                .as_ref()
                .unwrap()
                .evidence
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
    let evidence = &relay.snapshot.retry_assessment.as_ref().unwrap().evidence;
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

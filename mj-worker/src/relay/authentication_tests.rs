use super::{test_support::*, *};
use mj_core::config::HarnessKind;

fn open(root: &Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(HarnessKind::Claude);
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    relay
}

fn auth_failure(relay: &mut DurableRelay, id: &str, login: &str) {
    let mut diagnostic = mj_core::diagnostic::TurnDiagnostic::from_acp(
        &agent_client_protocol::Error::auth_required(),
    );
    diagnostic.login_fingerprint = Some(login.into());
    relay
        .record_command_completed(
            id,
            RelayCommandOutcome::Prompt {
                work_facts: None,
                stop_reason: "error".into(),
                diagnostic: Some(diagnostic),
                usage: None,
            },
        )
        .unwrap();
}

// Hard-won: #1515: a refreshed login left an auth-failed turn idle forever.
#[test]
fn authentication_recovery_survives_restart_and_never_retries_the_same_login() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("finish this task"));
    relay.claim_pending_commands(true).unwrap();
    auth_failure(&mut relay, "user-request", "old-access-hash");
    let (ordinal, _) = relay
        .authentication_retry_identity()
        .expect("auth failure is recoverable");
    assert!(
        !relay
            .submit_authentication_retry(ordinal, "old-access-hash")
            .unwrap()
    );
    drop(relay);

    let mut relay = open(root.path());
    assert!(
        relay
            .submit_authentication_retry(ordinal, "new-access-hash")
            .unwrap()
    );
    assert!(
        !relay
            .submit_authentication_retry(ordinal, "new-access-hash")
            .unwrap()
    );
    assert_eq!(
        relay.snapshot.continuation.user_command_id.as_deref(),
        Some("user-request")
    );
    let retry = relay.claim_pending_commands(true).unwrap().pop().unwrap();
    assert_eq!(retry.command_id, format!("auth-retry-{ordinal}"));
    assert_eq!(
        retry.command.prompt_blocks().unwrap().as_ref(),
        mj_core::continuation::prompt_blocks()
    );
    auth_failure(&mut relay, &retry.command_id, "new-access-hash");
    let (second, _) = relay.authentication_retry_identity().unwrap();
    assert!(
        !relay
            .submit_authentication_retry(second, "new-access-hash")
            .unwrap()
    );
    assert!(
        !relay
            .submit_authentication_retry(ordinal, "third-access-hash")
            .unwrap()
    );
    assert!(
        relay
            .submit_authentication_retry(second, "third-access-hash")
            .unwrap()
    );
}

#[test]
fn authentication_recovery_yields_to_new_input_cancellation_and_checkpoints() {
    for intervention in [
        "new input",
        "removed input",
        "cancel",
        "checkpoint",
        "harness turn",
        "question",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        submit_relay(&mut relay, "user-request", prompt("finish this task"));
        relay.claim_pending_commands(true).unwrap();
        if intervention == "cancel" {
            submit_relay(&mut relay, "cancel-request", RelayCommand::CancelTurn);
        }
        if intervention == "removed input" {
            submit_relay(&mut relay, "new-request", prompt("stop and explain"));
            submit_relay(
                &mut relay,
                "remove-new",
                RelayCommand::RemoveQueuedPrompt {
                    queued_command_id: "new-request".into(),
                },
            );
        }
        auth_failure(&mut relay, "user-request", "old-access-hash");
        let ordinal = relay.snapshot.handled_commands["user-request"]
            .terminal_ordinal
            .unwrap();
        match intervention {
            "new input" => {
                submit_relay(&mut relay, "new-request", prompt("stop and explain"));
            }
            "checkpoint" => {
                submit_relay(
                    &mut relay,
                    "checkpoint-request",
                    RelayCommand::BeginCheckpoint { reason: None },
                );
            }
            "harness turn" => {
                relay
                    .record_observation(RelayObservation::HarnessTurnStarted {
                        started_at_ms: epoch_millis(),
                    })
                    .unwrap();
            }
            "question" => {
                relay
                    .record_observation(RelayObservation::ElicitationRequested {
                        request: mj_core::elicitation::ElicitationRequest::from_acp_params(
                            "pending-question",
                            serde_json::json!({"mode":"form", "sessionId":SESSION, "message":"Which target?", "requestedSchema":{"type":"object", "properties":{}}}),
                        ).unwrap(),
                    }).unwrap();
            }
            _ => {}
        }
        assert!(
            !relay
                .submit_authentication_retry(ordinal, "new-access-hash")
                .unwrap(),
            "{intervention}"
        );
        if intervention == "checkpoint" {
            relay.claim_pending_commands(true).unwrap();
            relay.record_checkpoint_ready("checkpoint-request").unwrap();
            submit_relay(
                &mut relay,
                "release-checkpoint",
                RelayCommand::ReleaseCheckpoint {
                    barrier_command_id: "checkpoint-request".into(),
                },
            );
            assert!(
                relay
                    .submit_authentication_retry(ordinal, "new-access-hash")
                    .unwrap()
            );
        }
    }
}

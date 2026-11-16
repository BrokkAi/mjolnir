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
// Hard-won: #1547: a spawned sub-agent's model configure set quota_suppressed, so #1515 never retried its auth-failed turn.
#[test]
fn authentication_recovery_survives_restart_and_never_retries_the_same_login() {
    for original in ["user-request", "subagent-spawn-auth-regression"] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        if original.starts_with("subagent-spawn") {
            // A spawn configures the model before its generated prompt.
            submit_relay(
                &mut relay,
                "subagent-spawn-auth-regression-model",
                RelayCommand::SetConfig {
                    key: "model".into(),
                    value: "sonnet".into(),
                },
            );
            relay.claim_pending_commands(true).unwrap();
            relay
                .record_command_completed(
                    "subagent-spawn-auth-regression-model",
                    RelayCommandOutcome::Configured,
                )
                .unwrap();
        }
        submit_relay(&mut relay, original, prompt("finish this task"));
        relay.claim_pending_commands(true).unwrap();
        let authorization = relay.snapshot.continuation.user_command_id.clone();
        auth_failure(&mut relay, original, "old-access-hash");
        let (ordinal, _) = relay
            .authentication_retry_identity()
            .expect("auth failure is recoverable");
        // The worker publishes that it is waiting for a different login.
        assert_eq!(
            relay
                .operational_state()
                .authentication_retry_waiting
                .as_deref(),
            Some("old-access-hash")
        );
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
            authorization.as_deref()
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
        // The worker stops publishing it the moment the retry is no longer its to make.
        assert_eq!(
            relay.operational_state().authentication_retry_waiting,
            None,
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

#[test]
fn a_revision_24_snapshot_with_the_quota_flag_keeps_the_auth_retry_blocked() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("finish this task"));
    relay.claim_pending_commands(true).unwrap();
    auth_failure(&mut relay, "user-request", "old-access-hash");
    assert!(relay.authentication_retry_identity().is_some());
    let frontier = relay.snapshot.latest_ordinal;
    // What an older worker wrote after the user cancelled: the flag alone.
    let mut legacy = serde_json::to_value(&relay.snapshot).unwrap();
    legacy["format_version"] = serde_json::json!(24);
    legacy["continuation"]["quota_suppressed"] = serde_json::json!(true);
    legacy["continuation"]
        .as_object_mut()
        .unwrap()
        .remove("withdrawn_ordinal");
    drop(relay);
    std::fs::write(
        root.path().join(mj_core::relay::RELAY_STATE_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();

    let upgraded = open(root.path());
    assert_eq!(
        upgraded.snapshot.continuation.withdrawn_ordinal,
        Some(frontier)
    );
    assert!(upgraded.authentication_retry_identity().is_none());
}

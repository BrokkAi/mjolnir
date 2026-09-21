use super::{test_support::*, *};

fn completed(relay: &mut DurableRelay, id: &str) {
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert!(claimed.iter().any(|c| c.command_id == id));
    relay
        .record_command_completed(
            id,
            RelayCommandOutcome::Prompt {
                stop_reason: "end_turn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
}
fn request(relay: &DurableRelay) -> RelayCommand {
    let s = relay.operational_state();
    RelayCommand::ContinueAuthorizedWork {
        expected: RelayCursor {
            ordinal: s.latest_ordinal,
            digest: s.latest_digest,
        },
        user_command_id: s.continuation.user_command_id.unwrap(),
        completed_command_id: s.continuation.completed_command_id.unwrap(),
        attempt: s.continuation.attempts + 1,
    }
}
fn open(root: &std::path::Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "test").unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    relay
}
#[test]
fn continuation_admits_three_fixed_prompts_then_requires_new_user_input() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "original-prompt", prompt("Implement and test"));
    completed(&mut relay, "original-prompt");
    for attempt in 1..=3 {
        let id = format!("auto-continue-{attempt}");
        let cmd = request(&relay);
        assert!(relay.submit_command(&id, cmd.clone()).unwrap().is_ok());
        // Repeated delivery is an acknowledgement, not another attempt.
        assert!(relay.submit_command(&id, cmd).unwrap().is_ok());
        assert_eq!(relay.snapshot.continuation.attempts, attempt);
        assert_eq!(
            relay.snapshot.active_prompt.as_ref().unwrap().prompt,
            mj_core::continuation::prompt_blocks()
        );
        completed(&mut relay, &id);
    }
    assert!(
        relay
            .submit_command("auto-continue-four", request(&relay))
            .unwrap()
            .is_err()
    );
    drop(relay);
    let mut relay = open(root.path());
    assert_eq!(relay.snapshot.continuation.attempts, 3);
    assert!(
        relay
            .submit_command("auto-continue-after-restart", request(&relay))
            .unwrap()
            .is_err()
    );
    submit_relay(
        &mut relay,
        "another-user-prompt",
        prompt("Now also document it"),
    );
    completed(&mut relay, "another-user-prompt");
    assert_eq!(relay.snapshot.continuation.attempts, 0);
    assert!(
        relay
            .submit_command("auto-continue-new-task", request(&relay))
            .unwrap()
            .is_ok()
    );
}
#[test]
fn continuation_rejects_stale_evidence_and_active_work() {
    for action in ["prompt", "cancel", "checkpoint", "background"] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        submit_relay(&mut relay, "original-prompt", prompt("Implement and test"));
        completed(&mut relay, "original-prompt");
        let old = request(&relay);
        match action {
            "prompt" => {
                submit_relay(
                    &mut relay,
                    "new-user-message",
                    prompt("Stop; explain first"),
                );
            }
            "cancel" => {
                submit_relay(&mut relay, "cancel-the-turn", RelayCommand::CancelTurn);
            }
            "checkpoint" => {
                submit_relay(
                    &mut relay,
                    "checkpoint-test",
                    RelayCommand::BeginCheckpoint { reason: None },
                );
            }
            _ => {
                relay
                    .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
                    .unwrap();
            }
        }
        assert!(
            relay
                .submit_command("auto-continue-stale", old)
                .unwrap()
                .is_err(),
            "{action}"
        );
        assert_eq!(relay.snapshot.continuation.attempts, 0);
    }
}
#[test]
fn generated_prompts_neither_renew_allowance_nor_grant_authorization() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "capacity-retry-123", prompt("Continue"));
    completed(&mut relay, "capacity-retry-123");
    assert!(!relay.snapshot.continuation.eligible());
    submit_relay(&mut relay, "original-prompt", prompt("Implement and test"));
    completed(&mut relay, "original-prompt");
    relay
        .submit_command("auto-continue-first", request(&relay))
        .unwrap()
        .unwrap();
    completed(&mut relay, "auto-continue-first");
    submit_relay(&mut relay, "review-forward-123", prompt("Fix this finding"));
    completed(&mut relay, "review-forward-123");
    assert_eq!(relay.snapshot.continuation.attempts, 1);
    assert_eq!(
        relay.snapshot.continuation.user_command_id.as_deref(),
        Some("original-prompt")
    );
}

fn quota_schedule(relay: &DurableRelay, reset_at_ms: Option<i64>) -> RelayCommand {
    let s = relay.operational_state();
    RelayCommand::SetQuotaRecovery {
        expected: RelayCursor {
            ordinal: s.latest_ordinal,
            digest: s.latest_digest,
        },
        recovery: Some(Box::new(mj_core::continuation::QuotaRecovery {
            user_command_id: s.continuation.user_command_id.unwrap(),
            completed_command_id: s.continuation.completed_command_id.unwrap(),
            profile_id: "isolated-quota-test".into(),
            reset_at_ms,
            retry_at_ms: reset_at_ms.map(|t| t + 60_000),
            notice: "Quota reset scheduled".into(),
            submitted: false,
        })),
    }
}
fn quota_resume(relay: &DurableRelay) -> RelayCommand {
    let s = relay.operational_state();
    RelayCommand::ResumeAfterQuota {
        expected: RelayCursor {
            ordinal: s.latest_ordinal,
            digest: s.latest_digest,
        },
        completed_command_id: s.continuation.completed_command_id.unwrap(),
    }
}

#[test]
fn quota_retry_survives_restart_and_does_not_consume_or_renew_ordinary_allowance() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement and test"));
    completed(&mut relay, "user-request");
    for attempt in 1..=3 {
        let id = format!("auto-continue-{attempt}");
        let cmd = request(&relay);
        submit_relay(&mut relay, &id, cmd);
        completed(&mut relay, &id);
    }
    let cmd = quota_schedule(&relay, Some(epoch_millis() - 60_001));
    submit_relay(&mut relay, "quota-schedule", cmd);
    drop(relay);
    let mut relay = open(root.path());
    assert!(relay.snapshot.continuation.quota_recovery.is_some());
    let cmd = quota_resume(&relay);
    submit_relay(&mut relay, "quota-retry-1", cmd.clone());
    submit_relay(&mut relay, "quota-retry-1", cmd);
    assert_eq!(relay.snapshot.continuation.attempts, 3);
    assert_eq!(
        relay.snapshot.continuation.user_command_id.as_deref(),
        Some("user-request")
    );
    completed(&mut relay, "quota-retry-1");
    assert_eq!(
        relay.snapshot.continuation.completed_command_id.as_deref(),
        Some("quota-retry-1")
    );
    assert!(!relay.snapshot.continuation.eligible());
    assert!(
        relay
            .submit_command("duplicate-quota", quota_resume(&relay))
            .unwrap()
            .is_err()
    );
    let cmd = quota_schedule(&relay, Some(epoch_millis() - 60_001));
    submit_relay(&mut relay, "quota-schedule-next", cmd);
    let cmd = quota_resume(&relay);
    submit_relay(&mut relay, "quota-retry-2", cmd);
    assert_eq!(relay.snapshot.continuation.attempts, 3);
}

#[test]
fn quota_retry_requires_a_due_deadline_and_is_cancelled_by_user_work() {
    for action in ["early", "unknown", "cancel", "prompt", "checkpoint"] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        submit_relay(&mut relay, "user-request", prompt("Implement"));
        completed(&mut relay, "user-request");
        let reset = match action {
            "early" => Some(epoch_millis()),
            "unknown" => None,
            _ => Some(epoch_millis() - 60_001),
        };
        let cmd = quota_schedule(&relay, reset);
        submit_relay(&mut relay, "quota-schedule", cmd);
        let retry = quota_resume(&relay);
        match action {
            "cancel" => submit_relay(&mut relay, "cancel-quota", RelayCommand::Cancel),
            "prompt" => submit_relay(&mut relay, "new-user", prompt("Stop; explain first")),
            "checkpoint" => submit_relay(
                &mut relay,
                "checkpoint",
                RelayCommand::BeginCheckpoint { reason: None },
            ),
            _ => 0,
        };
        assert!(
            relay.submit_command("quota-retry", retry).unwrap().is_err(),
            "{action}"
        );
        if matches!(action, "cancel" | "prompt" | "checkpoint") {
            assert!(relay.snapshot.continuation.quota_recovery.is_none());
        }
        assert_eq!(relay.snapshot.continuation.attempts, 0);
    }
}

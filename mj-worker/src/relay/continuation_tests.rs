use super::{test_support::*, *};

fn completed(relay: &mut DurableRelay, id: &str) {
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert!(claimed.iter().any(|c| c.command_id == id));
    relay
        .record_command_completed(
            id,
            RelayCommandOutcome::Prompt {
                work_facts: None,
                stop_reason: "end_turn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
}
fn authorize(relay: &mut DurableRelay, action: mj_core::assessment::Action) {
    if let Some(mut a) = relay
        .snapshot
        .assessment
        .as_deref()
        .cloned()
        .filter(|a| a.current())
    {
        a.action = Some(action);
        a.status = mj_core::assessment::Status::Deferred;
        relay
            .append_relay_event(
                None,
                RelayObservation::TurnAssessmentUpdated {
                    assessment: Box::new(a),
                },
            )
            .unwrap();
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
fn generated_prompts_do_not_grant_authorization() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "capacity-retry-123", prompt("Continue"));
    completed(&mut relay, "capacity-retry-123");
    assert!(relay.snapshot.continuation.user_command_id.is_none());
    submit_relay(&mut relay, "original-prompt", prompt("Implement and test"));
    completed(&mut relay, "original-prompt");
    submit_relay(&mut relay, "review-forward-123", prompt("Fix this finding"));
    completed(&mut relay, "review-forward-123");
    // Hard-won: 8b08e8bc1: handback reminders and peer-session messages counted as user authorization
    for id in [
        mj_core::subagent::handback_reminder_command_id(7),
        mj_core::continuation::GeneratedPrompt::SessionMessageTurn.command_id("peer-1"),
    ] {
        submit_relay(&mut relay, &id, prompt("Please also update the docs"));
        completed(&mut relay, &id);
    }
    assert_eq!(
        relay.snapshot.continuation.user_command_id.as_deref(),
        Some("original-prompt")
    );
}

fn quota_schedule(relay: &mut DurableRelay, reset_at_ms: Option<i64>) -> RelayCommand {
    authorize(relay, mj_core::assessment::Action::RecoverQuota);
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

// Hard-won: 3c8973b87: a mailbox wake hit Kimi's quota with no recoverable request after restore.
#[test]
fn mailbox_quota_recovery_survives_missing_user_request_and_prior_cancellation() {
    // Hard-won: 9c2b74a01: a peer message delivered as a prompt lost the wake's quota identity.
    for (no_user_request, as_prompt) in [(true, false), (false, false), (true, true), (false, true)]
    {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        if !no_user_request {
            submit_relay(&mut relay, "user-request", prompt("Implement and test"));
            completed(&mut relay, "user-request");
        }
        submit_relay(&mut relay, "cancel-old-work", RelayCommand::CancelTurn);
        relay.claim_pending_commands(true).unwrap();
        relay
            .record_command_completed("cancel-old-work", RelayCommandOutcome::Cancelled)
            .unwrap();
        assert!(relay.snapshot.continuation.quota_suppressed);
        let wake_id = if as_prompt {
            // Workers without mailbox delivery get the message as a prompt.
            let id =
                mj_core::continuation::GeneratedPrompt::SessionMessageTurn.command_id("peer-1");
            submit_relay(&mut relay, &id, prompt("Your PR was merged."));
            let claimed = relay.claim_pending_commands(true).unwrap();
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].command_id, id);
            id
        } else {
            let event = mj_core::mailbox::MailboxEvent {
                key: "merged-pr".into(),
                source: "github".into(),
                wake: true,
                created_at_ms: 1,
                body: mj_core::mailbox::MailboxEventBody::PlainText {
                    text: "Your PR was merged.".into(),
                },
            };
            submit_relay(
                &mut relay,
                "deliver-merge",
                RelayCommand::DeliverMailboxEvent { event },
            );
            let claimed = relay.claim_pending_commands(true).unwrap();
            assert_eq!(claimed.len(), 1);
            assert!(matches!(
                claimed[0].command,
                RelayCommand::MailboxWake { .. }
            ));
            claimed[0].command_id.clone()
        };
        relay
            .record_command_completed(
                &wake_id,
                RelayCommandOutcome::Prompt {
                    work_facts: None,
                    stop_reason: mj_core::diagnostic::QUOTA_STOP_REASON.into(),
                    usage: None,
                    diagnostic: Some(mj_core::diagnostic::TurnDiagnostic {
                        message: "429 You've reached your 5-hour usage limit. Your quota will reset when the current 5-hour window ends.".into(),
                        code: Some("provider.api_error".into()),
                        http_status: Some(429),
                        reset_at: None,
                        login_fingerprint: None,
                    }),
                },
            )
            .unwrap();
        drop(relay);
        let mut relay = open(root.path());
        assert_eq!(
            relay.snapshot.continuation.completed_command_id.as_deref(),
            Some(wake_id.as_str())
        );
        assert_eq!(
            relay.snapshot.continuation.user_command_id.as_deref(),
            Some(if no_user_request {
                wake_id.as_str()
            } else {
                "user-request"
            })
        );
        let cmd = quota_schedule(&mut relay, Some(epoch_millis() - 60_001));
        submit_relay(&mut relay, "quota-schedule", cmd);
        drop(relay);
        let mut relay = open(root.path());
        let cmd = quota_resume(&relay);
        submit_relay(&mut relay, "quota-retry", cmd);
        completed(&mut relay, "quota-retry");
    }
}

#[test]
fn quota_retry_survives_restart_and_a_duplicate_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement and test"));
    completed(&mut relay, "user-request");
    let cmd = quota_schedule(&mut relay, Some(epoch_millis() - 60_001));
    submit_relay(&mut relay, "quota-schedule", cmd);
    drop(relay);
    let mut relay = open(root.path());
    assert!(relay.snapshot.continuation.quota_recovery.is_some());
    let cmd = quota_resume(&relay);
    submit_relay(&mut relay, "quota-retry-1", cmd.clone());
    submit_relay(&mut relay, "quota-retry-1", cmd);
    assert_eq!(
        relay.snapshot.continuation.user_command_id.as_deref(),
        Some("user-request")
    );
    completed(&mut relay, "quota-retry-1");
    assert_eq!(
        relay.snapshot.continuation.completed_command_id.as_deref(),
        Some("quota-retry-1")
    );
    assert!(
        relay
            .submit_command("duplicate-quota", quota_resume(&relay))
            .unwrap()
            .is_err()
    );
    let cmd = quota_schedule(&mut relay, Some(epoch_millis() - 60_001));
    submit_relay(&mut relay, "quota-schedule-next", cmd);
    let cmd = quota_resume(&relay);
    submit_relay(&mut relay, "quota-retry-2", cmd);
}

#[test]
fn quota_retry_requires_a_due_deadline_and_is_cancelled_by_user_work() {
    for action in ["early", "unknown", "cancel", "prompt"] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        submit_relay(&mut relay, "user-request", prompt("Implement"));
        completed(&mut relay, "user-request");
        let reset = match action {
            "early" => Some(epoch_millis()),
            "unknown" => None,
            _ => Some(epoch_millis() - 60_001),
        };
        let cmd = quota_schedule(&mut relay, reset);
        submit_relay(&mut relay, "quota-schedule", cmd);
        let retry = quota_resume(&relay);
        match action {
            "cancel" => submit_relay(&mut relay, "cancel-quota", RelayCommand::Cancel),
            "prompt" => submit_relay(&mut relay, "new-user", prompt("Stop; explain first")),
            _ => 0,
        };
        assert!(
            relay.submit_command("quota-retry", retry).unwrap().is_err(),
            "{action}"
        );
        if matches!(action, "cancel" | "prompt") {
            assert!(relay.snapshot.continuation.quota_recovery.is_none());
        }
    }
}

/// F18, session e33442fe: a quota refusal ended three turns the harness had
/// started on its own, each scheduled a recovery, and a routine checkpoint
/// then cleared the recovery and set `quota_suppressed`, so nothing resumed at
/// the deadline. A checkpoint barrier is housekeeping, not the user taking
/// over: the recovery survives it and is submittable once the barrier lifts.
#[test]
fn a_routine_checkpoint_keeps_the_quota_recovery_of_a_self_started_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement"));
    completed(&mut relay, "user-request");
    let start = self_started_turn(&mut relay);
    // The refused turn is over: nothing about it may hold `can_submit` false.
    let facts = relay.activity_facts();
    assert!(facts.harness_turn_started_at_ms.is_none());
    assert!(facts.tools_in_flight.is_empty());
    assert!(mj_core::activity::can_submit(&facts));
    let cmd = quota_schedule(&mut relay, Some(epoch_millis() - 60_001));
    submit_relay(&mut relay, "quota-schedule", cmd);
    assert_eq!(
        relay
            .snapshot
            .continuation
            .quota_recovery
            .as_ref()
            .map(|r| r.completed_command_id.clone()),
        Some(mj_core::continuation::harness_turn_id(start))
    );

    ready_checkpoint(&mut relay, "routine-checkpoint");
    let held = relay.snapshot.continuation.clone();
    assert!(held.quota_recovery.is_some(), "the checkpoint kept it");
    assert!(!held.quota_suppressed);
    // While the barrier stands the resume waits, as it must.
    assert!(!mj_core::activity::can_submit(&relay.activity_facts()));
    submit_relay(
        &mut relay,
        "release-routine-checkpoint",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "routine-checkpoint".into(),
        },
    );
    assert!(mj_core::activity::can_submit(&relay.activity_facts()));
    let resume = quota_resume(&relay);
    assert!(relay.submit_command("quota-retry", resume).unwrap().is_ok());
}

/// Opens and ends a turn the harness starts on its own; returns its start ordinal.
fn self_started_turn(relay: &mut DurableRelay) -> u64 {
    relay
        .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
        .unwrap();
    let start = relay.snapshot.latest_ordinal;
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            autonomous: None,
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    start
}

#[test]
fn a_self_started_turn_becomes_the_completed_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement and test"));
    completed(&mut relay, "user-request");

    let start = self_started_turn(&mut relay);
    let c = &relay.snapshot.continuation;
    let id = mj_core::continuation::harness_turn_id(start);
    assert_eq!(c.completed_command_id.as_deref(), Some(id.as_str()));
    let turn = c.harness_turn.as_ref().unwrap();
    assert_eq!(
        (turn.start_position, turn.id.as_str()),
        (start, id.as_str())
    );
    assert_eq!(c.user_command_id.as_deref(), Some("user-request"));
}

#[test]
fn a_start_inside_our_prompt_is_not_a_turn_of_its_own() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement"));
    relay.claim_pending_commands(true).unwrap();
    // Codex reports native execution for ordinary replies too, and it can
    // outlast the prompt's own result.
    relay
        .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
        .unwrap();
    relay
        .record_command_completed(
            "user-request",
            RelayCommandOutcome::Prompt {
                work_facts: None,
                stop_reason: "end_turn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            autonomous: None,
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    let c = &relay.snapshot.continuation;
    assert_eq!(c.completed_command_id.as_deref(), Some("user-request"));
    assert!(c.harness_turn.is_none());
}

#[test]
fn a_second_self_started_turn_clears_a_waiting_recovery_and_its_end_can_schedule_again() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement"));
    completed(&mut relay, "user-request");
    self_started_turn(&mut relay);
    let cmd = quota_schedule(&mut relay, Some(epoch_millis() + 3_600_000));
    submit_relay(&mut relay, "quota-schedule-1", cmd);
    assert!(relay.snapshot.continuation.quota_recovery.is_some());

    let second = self_started_turn(&mut relay);
    let c = &relay.snapshot.continuation;
    assert!(c.quota_recovery.is_none());
    assert!(
        !c.quota_suppressed,
        "the new turn's own check may schedule again"
    );
    assert_eq!(
        c.completed_command_id,
        Some(mj_core::continuation::harness_turn_id(second))
    );
    let cmd = quota_schedule(&mut relay, Some(epoch_millis() + 3_600_000));
    submit_relay(&mut relay, "quota-schedule-2", cmd);
    assert_eq!(
        relay
            .snapshot
            .continuation
            .quota_recovery
            .as_ref()
            .unwrap()
            .completed_command_id,
        mj_core::continuation::harness_turn_id(second)
    );
}

fn limited_goal(relay: &mut DurableRelay, reason: &str) {
    relay.snapshot.goal.capability = Some(mj_core::goal::GoalCapability {
        version: 1,
        control_method: "_session/goal".into(),
        actions: vec!["resume".into(), "pause".into()],
    });
    relay.snapshot.goal.snapshot = Some(mj_core::goal::GoalSnapshot {
        objective: "Finish the campaign".into(),
        status: "limited".into(),
        created_at: Some(100_000),
        control_method: Some("_session/goal".into()),
        details: [("limitReason".to_owned(), serde_json::json!(reason))].into(),
    });
}

#[test]
fn a_due_recovery_resumes_a_usage_limited_goal_and_a_spent_budget_blocks_everything() {
    for reason in ["usage", "budget"] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        submit_relay(&mut relay, "user-request", prompt("Implement"));
        completed(&mut relay, "user-request");
        self_started_turn(&mut relay);
        let cmd = quota_schedule(&mut relay, Some(epoch_millis() - 60_001));
        submit_relay(&mut relay, "quota-schedule", cmd);
        limited_goal(&mut relay, reason);
        let resume = RelayCommand::GoalControl {
            action: mj_core::goal::GoalControlAction::Resume,
        };
        let id = format!("{}7", mj_core::continuation::QUOTA_GOAL_RESUME_PREFIX);
        let result = relay.submit_command(&id, resume).unwrap();
        if reason == "budget" {
            assert!(result.is_err());
            assert!(
                relay
                    .submit_command("quota-retry-7", quota_resume(&relay))
                    .unwrap()
                    .is_err()
            );
            continue;
        }
        assert!(result.is_ok());
        let c = &relay.snapshot.continuation;
        assert!(c.quota_recovery.as_ref().unwrap().submitted);
        assert!(!c.quota_suppressed && !c.suppressed);
        assert!(c.completed_command_id.is_none());
    }
}

#[test]
fn a_users_own_goal_resume_still_takes_over_from_automation() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement"));
    completed(&mut relay, "user-request");
    limited_goal(&mut relay, "budget");
    submit_relay(
        &mut relay,
        "goal-resume-by-user",
        RelayCommand::GoalControl {
            action: mj_core::goal::GoalControlAction::Resume,
        },
    );
    let c = &relay.snapshot.continuation;
    assert!(c.suppressed && c.quota_suppressed);
}

fn handback_request(relay: &DurableRelay) -> RelayCommand {
    let turn = relay.snapshot.turn_completion.as_ref().unwrap();
    RelayCommand::HandbackReminder {
        completed_command_id: turn.command_id.clone(),
        completed_ordinal: turn.completed_ordinal,
    }
}

#[test]
fn handback_reminder_admission_is_durable_and_deduplicated() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "original-prompt", prompt("Write the report"));
    completed(&mut relay, "original-prompt");
    let request = handback_request(&relay);
    let accepted = relay
        .submit_command("handback-reminder-test", request.clone())
        .unwrap()
        .unwrap();
    assert_eq!(
        relay.snapshot.active_prompt.as_ref().unwrap().prompt,
        vec![agent_client_protocol::v1::schema::ContentBlock::from(
            mj_core::subagent::HANDBACK_REMINDER_TEXT.to_owned()
        )]
    );
    assert!(
        relay
            .snapshot
            .assessment_context
            .as_ref()
            .unwrap()
            .messages
            .iter()
            .all(|message| message.text != mj_core::subagent::HANDBACK_REMINDER_TEXT)
    );
    assert_eq!(
        relay.snapshot.continuation.user_command_id.as_deref(),
        Some("original-prompt")
    );
    assert_eq!(
        relay
            .submit_command("handback-reminder-test", request.clone())
            .unwrap()
            .unwrap(),
        accepted
    );
    drop(relay);
    let mut relay = open(root.path());
    assert_eq!(
        relay
            .submit_command("handback-reminder-test", request)
            .unwrap()
            .unwrap(),
        accepted
    );
}

#[test]
fn handback_reminder_rejects_newer_input_even_after_queue_removal_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "original-prompt", prompt("Write the report"));
    completed(&mut relay, "original-prompt");
    let request = handback_request(&relay);
    // Hold input behind a checkpoint so it can be removed before execution.
    ready_checkpoint(&mut relay, "checkpoint-hold");
    submit_relay(&mut relay, "newer-user-prompt", prompt("New instructions"));
    submit_relay(
        &mut relay,
        "remove-newer-prompt",
        RelayCommand::ClearQueuedPrompts,
    );
    submit_relay(
        &mut relay,
        "release-checkpoint",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "checkpoint-hold".into(),
        },
    );
    drop(relay);
    let mut relay = open(root.path());
    assert!(mj_core::activity::can_submit(&relay.activity_facts()));
    assert!(
        relay
            .submit_command("handback-reminder-stale", request)
            .unwrap()
            .is_err()
    );
}

#[test]
fn handback_reminder_rejects_a_different_completed_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "original-prompt", prompt("Write the report"));
    completed(&mut relay, "original-prompt");
    let request = handback_request(&relay);
    submit_relay(&mut relay, "newer-user-prompt", prompt("New instructions"));
    completed(&mut relay, "newer-user-prompt");
    assert!(
        relay
            .submit_command("handback-reminder-stale", request)
            .unwrap()
            .is_err()
    );
}

#[test]
fn handback_reminder_rejects_a_newer_native_turn_after_it_settles() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "original-prompt", prompt("Write the report"));
    completed(&mut relay, "original-prompt");
    let request = handback_request(&relay);
    relay
        .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
        .unwrap();
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            autonomous: None,
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    assert!(
        relay
            .submit_command("handback-reminder-stale", request)
            .unwrap()
            .is_err()
    );
}

#[test]
fn legacy_subagent_wait_journal_command_is_a_noop_after_replay() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "parent-turn", prompt("Delegate the work"));
    let command: RelayCommand = serde_json::from_value(
        serde_json::json!({"type":"set_subagent_wait","data":{"active_prompt_id":"parent-turn", "waiting":true}}),
    )
    .unwrap();
    submit_relay(&mut relay, "legacy-wait", command);
    assert!(
        relay
            .snapshot
            .continuation
            .awaiting_children_since_ms
            .is_none()
    );
    completed(&mut relay, "parent-turn");
    let mut legacy = serde_json::to_value(&relay.snapshot).unwrap();
    legacy["continuation"]["awaiting_children_since_ms"] = 123.into();
    let decoded: mj_core::relay::RelaySnapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.continuation.awaiting_children_since_ms, Some(123));
    drop(relay);
    let relay = open(root.path());
    assert!(
        relay
            .snapshot
            .continuation
            .awaiting_children_since_ms
            .is_none()
    );
}

#[test]
fn a_legacy_automatic_continuation_is_refused_live_but_its_journal_entry_still_replays() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement and test"));
    completed(&mut relay, "user-request");
    let s = relay.operational_state();
    let command = RelayCommand::ContinueAuthorizedWork {
        expected: RelayCursor {
            ordinal: s.latest_ordinal,
            digest: s.latest_digest,
        },
        user_command_id: "user-request".into(),
        completed_command_id: "user-request".into(),
        attempt: 2,
    };
    assert!(
        relay
            .submit_command("auto-continue-live", command.clone())
            .unwrap()
            .is_err()
    );
    // An older worker journalled the command before accepting it.
    relay
        .append_relay_event(
            Some("auto-continue-old"),
            RelayObservation::CommandQueued {
                command_id: "auto-continue-old".into(),
                command,
                created_at_ms: 1,
            },
        )
        .unwrap();
    let c = &relay.snapshot.continuation;
    assert_eq!((c.attempts, c.completed_command_id.as_deref()), (2, None));
    drop(relay);
    let relay = open(root.path());
    let c = &relay.snapshot.continuation;
    assert_eq!((c.attempts, c.completed_command_id.as_deref()), (2, None));
    assert_eq!(c.user_command_id.as_deref(), Some("user-request"));
}

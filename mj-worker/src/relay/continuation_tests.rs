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

/// Opens and ends a turn the harness starts on its own; returns its start ordinal.
fn self_started_turn(relay: &mut DurableRelay) -> u64 {
    relay
        .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
        .unwrap();
    let start = relay.snapshot.latest_ordinal;
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    start
}

#[test]
fn a_self_started_turn_becomes_the_completed_turn_without_renewing_the_allowance() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    submit_relay(&mut relay, "user-request", prompt("Implement and test"));
    completed(&mut relay, "user-request");
    let cmd = request(&relay);
    submit_relay(&mut relay, "auto-continue-1", cmd);
    completed(&mut relay, "auto-continue-1");

    let start = self_started_turn(&mut relay);
    let c = &relay.snapshot.continuation;
    let id = mj_core::continuation::harness_turn_id(start);
    assert_eq!(c.completed_command_id.as_deref(), Some(id.as_str()));
    let turn = c.harness_turn.as_ref().unwrap();
    assert_eq!(
        (turn.start_position, turn.id.as_str()),
        (start, id.as_str())
    );
    assert_eq!(c.attempts, 1);
    assert_eq!(c.user_command_id.as_deref(), Some("user-request"));

    let cmd = request(&relay);
    assert!(
        matches!(&cmd, RelayCommand::ContinueAuthorizedWork { completed_command_id, attempt: 2, .. } if *completed_command_id == id)
    );
    submit_relay(&mut relay, "auto-continue-2", cmd);
    assert!(relay.snapshot.continuation.harness_turn.is_none());
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
                stop_reason: "end_turn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
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
    let cmd = quota_schedule(&relay, Some(epoch_millis() + 3_600_000));
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
    let cmd = quota_schedule(&relay, Some(epoch_millis() + 3_600_000));
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

#[test]
fn background_work_holds_continuation_back_only_until_jev_judges_it_idle() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = open(root.path());
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
    relay.background_work = BackgroundWorkPolicy::ClaudeTasks;
    submit_relay(&mut relay, "user-request", prompt("Implement and test"));
    completed(&mut relay, "user-request");
    relay
        .claude_background_tasks_changed(vec![crate::acp::ClaudeBackgroundTask {
            task_id: "0".into(),
            description: "sleep infinity".into(),
        }])
        .unwrap();
    assert!(
        relay
            .submit_command("auto-continue-early", request(&relay))
            .unwrap()
            .is_err()
    );
    // A due quota recovery is recorded whatever keeps the session busy.
    let cmd = quota_schedule(&relay, Some(epoch_millis() + 3_600_000));
    assert!(relay.submit_command("quota-schedule", cmd).unwrap().is_ok());
    let cancel = RelayCommand::SetQuotaRecovery {
        expected: RelayCursor {
            ordinal: relay.snapshot.latest_ordinal,
            digest: relay.snapshot.latest_digest.clone(),
        },
        recovery: None,
    };
    submit_relay(&mut relay, "quota-cancel", cancel);

    let (generation, _, _) = relay.pending_replied_verdict().unwrap();
    assert_eq!(
        relay
            .apply_replied_decision(
                generation,
                mj_core::activity::verdict::Decision::InferIdle,
                200
            )
            .unwrap(),
        "applied"
    );
    assert!(!relay.operational_state().background_commands.is_empty());
    assert!(
        relay
            .submit_command("auto-continue-idle", request(&relay))
            .unwrap()
            .is_ok()
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
        let cmd = quota_schedule(&relay, Some(epoch_millis() - 60_001));
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
            assert!(
                relay
                    .submit_command("auto-continue-1", request(&relay))
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

use agent_client_protocol::schema::v1::{
    ContentBlock, TextContent, ToolCallUpdate, ToolCallUpdateFields,
};

use super::*;
use mj_core::relay::{RelayCommand, RelayCommandOutcome, RelayObservation, relay_event_digest};
use serde_json::json;

fn event(previous: &MaterializedSession, observation: RelayObservation) -> RelayEvent {
    let mut event = RelayEvent {
        format: mj_core::relay::RELAY_EVENT_FORMAT_V1,
        ordinal: previous.applied_event_ordinal + 1,
        previous_digest: previous.applied_event_digest.clone(),
        digest: String::new(),
        recorded_at_ms: 0,
        command_id: None,
        observation,
    };
    event.recorded_at_ms = i64::try_from(event.ordinal).unwrap() * 100;
    event.digest = relay_event_digest(&event).unwrap();
    event
}

fn apply(session: &mut MaterializedSession, event: RelayEvent) {
    let projected = project_relay_event(session, &event).unwrap();
    apply_committed_projection_event(session, &event, projected.mutation).unwrap();
}

fn apply_observation(session: &mut MaterializedSession, observation: RelayObservation) {
    let next = event(session, observation);
    apply(session, next);
}

fn rendered_materialized_transcript(session: &MaterializedSession) -> String {
    let title = session
        .resolved_title()
        .unwrap_or_else(|| session.session_id.clone());
    let rows = session
        .transcript
        .iter()
        .map(|item| {
            format!(
                "{}: {}",
                item.position,
                crate::transcript::transcript_item_text(item)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Session: {title}\n{rows}")
}

fn append_transcript_state(output: &mut String, label: &str, session: &MaterializedSession) {
    use std::fmt::Write as _;

    if !output.is_empty() {
        output.push('\n');
    }
    writeln!(output, "=== {label} (session transcript) ===").unwrap();
    output.push_str(&rendered_materialized_transcript(session));
    output.push('\n');
}

#[test]
fn golden_plan_proposal_transcript() {
    use std::fmt::Write as _;

    let mut output = String::new();
    let mut session = MaterializedSession::empty("session-1");
    let request = mj_core::acp::normalized_plan_review(
        "plan-review-3".into(),
        &serde_json::json!({ "plan": "1. Read the code\n2. Change it" }),
    );
    apply_observation(
        &mut session,
        RelayObservation::ElicitationRequested {
            request: request.clone(),
        },
    );
    append_transcript_state(&mut output, "decision pending", &session);
    writeln!(
        output,
        "pending decisions: {}",
        session.pending_elicitations.len()
    )
    .unwrap();

    apply_observation(
        &mut session,
        RelayObservation::ElicitationResolved {
            elicitation_id: "plan-review-3".into(),
            action: "accept".into(),
        },
    );
    append_transcript_state(&mut output, "proposal remains after accepting", &session);
    writeln!(
        output,
        "pending decisions: {}",
        session.pending_elicitations.len()
    )
    .unwrap();

    let mut conversation = MaterializedSession::empty("session-1");
    apply_observation(&mut conversation, untagged_agent_chunk("here is my plan"));
    apply_observation(
        &mut conversation,
        RelayObservation::ElicitationRequested {
            request: mj_core::acp::normalized_plan_review(
                "plan-review-1".into(),
                &serde_json::json!({ "plan": "do the work" }),
            ),
        },
    );
    apply_observation(&mut conversation, untagged_agent_chunk("starting now"));
    append_transcript_state(
        &mut output,
        "proposal stays between its conversation",
        &conversation,
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "plan-proposal-transcript",
        &output,
    );
}

#[test]
fn golden_session_title_projection() {
    let mut output = String::new();

    let mut provisional = MaterializedSession::empty("session-1");
    apply_observation(
        &mut provisional,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new(
                    "  fix the flaky\nresume test  ",
                ))],
            },
            created_at_ms: 100,
        },
    );
    append_transcript_state(
        &mut output,
        "first prompt supplies provisional title",
        &provisional,
    );

    let mut titled = MaterializedSession::empty("session-1");
    for (command_id, text, created_at_ms) in [
        ("prompt-1", "first prompt", 100),
        ("prompt-2", "second prompt", 200),
    ] {
        apply_observation(
            &mut titled,
            RelayObservation::CommandQueued {
                command_id: command_id.into(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new(text))],
                },
                created_at_ms,
            },
        );
    }
    apply_observation(
        &mut titled,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new()
                    .title("Agent-generated title"),
            )),
        },
    );
    append_transcript_state(
        &mut output,
        "harness title replaces provisional title",
        &titled,
    );

    let mut backfilled = MaterializedSession::empty("session-1");
    backfilled.transcript.push(Arc::new(TranscriptItem {
        stable_id: "user:prompt-1".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 100,
        last_changed_at_ms: 100,
        body: TranscriptBody::User {
            content: vec![
                serde_json::to_value(ContentBlock::Text(TextContent::new("original task")))
                    .unwrap(),
            ],
        },
    }));
    apply_observation(
        &mut backfilled,
        RelayObservation::CommandQueued {
            command_id: "prompt-2".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("follow-up task"))],
            },
            created_at_ms: 200,
        },
    );
    append_transcript_state(
        &mut output,
        "first existing prompt backfills title",
        &backfilled,
    );

    for (command_id, command, outcome) in [
        (
            "shell-exit",
            "cargo test -p mj-transcript",
            mj_core::relay::UserShellResult {
                command: "cargo test -p mj-transcript".into(),
                stdout: "2 passed".into(),
                stderr: "one expected warning".into(),
                stdout_truncated: false,
                stderr_truncated: false,
                exit_code: Some(7),
                signal: None,
                duration_ms: 1_234,
                status: mj_core::relay::UserShellStatus::Exited,
                error: None,
            },
        ),
        (
            "shell-signal",
            "long-running generator",
            mj_core::relay::UserShellResult {
                command: "long-running generator".into(),
                stdout: "partial output".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                exit_code: None,
                signal: Some("SIGTERM".into()),
                duration_ms: 2_345,
                status: mj_core::relay::UserShellStatus::Signaled,
                error: None,
            },
        ),
    ] {
        let mut shell = MaterializedSession::empty("session-1");
        apply_observation(
            &mut shell,
            RelayObservation::CommandQueued {
                command_id: command_id.into(),
                command: RelayCommand::RunUserShell {
                    command: command.into(),
                },
                created_at_ms: 300,
            },
        );
        apply_observation(
            &mut shell,
            RelayObservation::CommandCompleted {
                barrier_command_id: None,
                command: Some(mj_core::relay::RelayCommandKind::RunUserShell),
                command_id: command_id.into(),
                outcome: RelayCommandOutcome::UserShell { result: outcome },
            },
        );
        append_transcript_state(&mut output, command_id, &shell);
    }

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "session-title-projection",
        &output,
    );
}

#[test]
fn golden_mailbox_delivery_transcript() {
    let mut session = MaterializedSession::empty("session-mailbox");
    let github_event = mj_core::mailbox::MailboxEvent {
        key: "github:owner/repo#12:comment:345".into(),
        source: "github".into(),
        wake: false,
        text: "A review comment arrived.".into(),
        created_at_ms: 100,
    };
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "github-event".into(),
            command: RelayCommand::DeliverMailboxEvent {
                event: github_event,
            },
            created_at_ms: 100,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::MailboxEventsDelivered {
            event_keys: vec!["github:owner/repo#12:comment:345".into()],
            path: mj_core::mailbox::MailboxDeliveryPath::ToolHook,
            prompt_command_id: None,
            hook_event: Some("PostToolUse".into()),
        },
    );

    let parent_event = mj_core::mailbox::MailboxEvent {
        key: "parent:message:1".into(),
        source: "parent".into(),
        wake: false,
        text: "Please check this detail.".into(),
        created_at_ms: 300,
    };
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "parent-event".into(),
            command: RelayCommand::DeliverMailboxEvent {
                event: parent_event.clone(),
            },
            created_at_ms: 300,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::MailboxEventsDelivered {
            event_keys: vec![parent_event.key.clone()],
            path: mj_core::mailbox::MailboxDeliveryPath::Prompt,
            prompt_command_id: Some("user-prompt".into()),
            hook_event: None,
        },
    );

    let api_event = mj_core::mailbox::MailboxEvent {
        key: "api:urgent-1".into(),
        source: "api".into(),
        wake: true,
        text: "A new task is ready.".into(),
        created_at_ms: 500,
    };
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "api-event".into(),
            command: RelayCommand::DeliverMailboxEvent {
                event: api_event.clone(),
            },
            created_at_ms: 500,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "api-wake".into(),
            command: RelayCommand::MailboxWake {
                events: vec![api_event],
            },
            created_at_ms: 600,
        },
    );

    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "mailbox-delivery-transcript",
        &rendered_materialized_transcript(&session),
    );
}

// Hard-won: 60145fdb: leaving Claude Plan mode selected default Manual permissions instead of the saved policy.
#[test]
fn execution_mode_restoration_reports_durable_success_and_failure_to_clients() {
    let session = MaterializedSession::empty("restore-mode");
    for (observation, expected) in [
        (
            RelayObservation::CommandCompleted {
                barrier_command_id: None,
                command: Some(RelayCommandKind::RestoreExecutionMode),
                command_id: "restore-mode".into(),
                outcome: RelayCommandOutcome::Configured,
            },
            None,
        ),
        (
            RelayObservation::CommandRejected {
                reason: None,
                command_id: "restore-mode".into(),
                command: RelayCommandKind::RestoreExecutionMode,
                message: "auto mode refused".into(),
            },
            Some("auto mode refused".to_owned()),
        ),
    ] {
        let projected = project_relay_event(&session, &event(&session, observation)).unwrap();
        assert_eq!(
            projected.mutation.config_results,
            [("restore-mode".into(), expected)]
        );
    }
}

// Hard-won: 61a1cfaf: session loading discarded Codex goal and command-catalogue state across config replacement.
#[test]
fn configuration_replacement_preserves_goals_and_removes_obsolete_adapter_settings() {
    let mut session = MaterializedSession::empty("goal-config");
    apply_observation(&mut session, RelayObservation::SessionUpdate {
        update: Box::new(serde_json::from_value(json!({
            "sessionUpdate": "session_info_update", "_meta": {
                "mjGoalCapability": {"version": 1, "controlMethod": "_session/goal", "actions": ["pause", "resume", "clear"]},
                "goal": {"objective": "finish", "status": "paused", "createdAt": 1, "tokensUsed": 42, "tokenBudget": 1000},
                "execution": {"version": 1, "revision": 2, "status": "idle"},
                "mjGoalResumeAnswered": "answered"
            }
        })).unwrap()),
    });
    let mut goal = session.configuration.goal.clone().unwrap();
    goal.pending_resume = Some("pending".into());
    goal.decision = Some(mj_core::goal::GoalDecision {
        goal: goal.snapshot.clone().unwrap(),
        resume: true,
    });
    session.configuration.goal = Some(goal.clone());
    for via_notification in [false, true] {
        session
            .configuration
            .values
            .insert("obsolete".into(), json!(true));
        let options = json!([{
            "id": "model", "name": "Model", "type": "select", "currentValue": "astra",
            "options": [{"value": "astra", "name": "Astra"}]
        }]);
        let observation = if via_notification {
            RelayObservation::SessionUpdate {
                update: Box::new(
                    serde_json::from_value(json!({
                        "sessionUpdate": "config_option_update", "configOptions": options
                    }))
                    .unwrap(),
                ),
            }
        } else {
            RelayObservation::SessionConfigured {
                config_options: serde_json::from_value(options).unwrap(),
            }
        };
        apply_observation(&mut session, observation);
        assert_eq!(session.configuration.goal.as_ref(), Some(&goal));
        assert_eq!(
            session.configuration.values,
            BTreeMap::from([("model".into(), json!("astra"))])
        );

        let canonical = canonical_session_from_materialized(&session).unwrap();
        let archived = serde_json::to_value(&canonical).unwrap();
        assert_eq!(
            archived["session"]["configuration"]["mj_goal_state"],
            serde_json::to_value(&goal).unwrap()
        );
        let restored = materialized_session_from_canonical(
            "goal-config",
            &serde_json::from_value(archived).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.configuration, session.configuration);
    }
    let mut cleared = session.clone();
    apply_observation(
        &mut cleared,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "clear".into(),
            outcome: RelayCommandOutcome::ContextCleared {
                native_session_id: "new".into(),
                memory: None,
            },
        },
    );
    assert_eq!(
        cleared.configuration.goal,
        Some(Box::new(mj_core::goal::GoalState {
            capability: goal.capability.clone(),
            known: true,
            ..Default::default()
        }))
    );
    assert_eq!(cleared.configuration.values, session.configuration.values);
    apply_observation(&mut session, RelayObservation::SessionRestarted);
    goal.restart();
    assert_eq!(session.configuration.goal, Some(goal));
    assert_eq!(session.configuration.values["model"], json!("astra"));
}

fn apply_indexed_observation(
    session: &mut MaterializedSession,
    index: &mut ProjectionIndex,
    observation: RelayObservation,
) {
    let next = event(session, observation);
    let projected = project_relay_event_indexed(session, index, &next).unwrap();
    apply_committed_projection_event_indexed(session, index, &next, projected.mutation).unwrap();
}

#[test]
fn resume_open_updates_operational_state_without_adding_transcript_noise() {
    let session = MaterializedSession::empty("session");
    let resumed = event(
        &session,
        RelayObservation::SessionOpened {
            native_session_id: "native".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        },
    );
    let mutation = project_relay_event(&session, &resumed).unwrap().mutation;
    assert!(mutation.transcript.is_empty());
    assert_eq!(mutation.pending_elicitations, Some(Vec::new()));

    let started = event(
        &session,
        RelayObservation::SessionOpened {
            native_session_id: "native".into(),
            resumed: false,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        },
    );
    assert_eq!(
        project_relay_event(&session, &started)
            .unwrap()
            .mutation
            .transcript
            .len(),
        1
    );
}

/// Starts `prompt-1`, queues `prompt-2` behind it, and admits a steer of
/// `prompt-2` into the running turn. Returns the two acceptance ordinals.
fn start_prompt_and_queue_a_steer(session: &mut MaterializedSession) -> (u64, u64) {
    let queue = |session: &mut MaterializedSession, id: &str| {
        apply_observation(
            session,
            RelayObservation::CommandQueued {
                command_id: id.into(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::from(id)],
                },
                created_at_ms: 10,
            },
        );
        session.applied_event_ordinal
    };
    let first = queue(session, "prompt-1");
    apply_observation(
        session,
        RelayObservation::CommandStarted {
            command_id: "prompt-1".into(),
            started_at_ms: 20,
        },
    );
    let second = queue(session, "prompt-2");
    apply_observation(
        session,
        RelayObservation::CommandQueued {
            command_id: "steer-1".into(),
            command: RelayCommand::Steer {
                active_prompt_id: "prompt-1".into(),
                queued_prompt_id: "prompt-2".into(),
            },
            created_at_ms: 30,
        },
    );
    (first, second)
}

fn steer_prompt_2(session: &mut MaterializedSession) {
    apply_observation(
        session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "steer-1".into(),
            outcome: RelayCommandOutcome::Steered {
                queued_command_id: "prompt-2".into(),
            },
        },
    );
}

#[test]
fn a_steered_prompt_finishes_with_the_turn_it_joined() {
    let mut session = MaterializedSession::empty("session");
    let (first, second) = start_prompt_and_queue_a_steer(&mut session);
    steer_prompt_2(&mut session);
    let turn = session.active_turn.clone().expect("the running turn");
    assert_eq!(turn.command_id, "prompt-2");
    assert_eq!(turn.steered_into.as_deref(), Some("prompt-1"));

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "prompt-1".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "EndTurn".into(),
                usage: None,
            },
        },
    );
    assert!(session.active_turn.is_none());
    let outcome = session.last_turn_outcome.clone().expect("an outcome");
    assert_eq!(outcome.command_id, "prompt-1");
    // `mj wait --turn` finishes once an outcome's acceptance ordinal reaches
    // its target, so a waiter for either prompt now finishes.
    assert_eq!(outcome.accepted_ordinal, Some(second));
    assert!(second > first);
    assert_eq!(outcome.turn_start_position, Some(turn.turn_start_position));
}

#[test]
fn an_interrupted_steered_turn_clears_the_running_turn() {
    let mut session = MaterializedSession::empty("session");
    let (_, second) = start_prompt_and_queue_a_steer(&mut session);
    steer_prompt_2(&mut session);

    apply_observation(
        &mut session,
        RelayObservation::CommandInterrupted {
            reason: None,
            command_id: "prompt-1".into(),
            command: RelayCommandKind::Prompt,
            message: "stopped".into(),
        },
    );
    assert!(session.active_turn.is_none());
    let outcome = session.last_turn_outcome.clone().expect("an outcome");
    assert_eq!(outcome.accepted_ordinal, Some(second));
    assert_eq!(
        outcome.outcome,
        TurnOutcomeKind::Interrupted {
            reason: None,
            message: "stopped".into()
        }
    );
}

#[test]
fn a_returned_steer_leaves_the_prompt_queued_and_the_turn_running() {
    let mut session = MaterializedSession::empty("session");
    start_prompt_and_queue_a_steer(&mut session);
    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "steer-1".into(),
            outcome: RelayCommandOutcome::SteeringReturned {
                queued_command_id: "prompt-2".into(),
            },
        },
    );
    assert_eq!(
        session
            .active_turn
            .as_ref()
            .map(|turn| turn.command_id.as_str()),
        Some("prompt-1")
    );
    assert_eq!(session.queued_prompts.len(), 1);
    assert_eq!(session.queued_prompts[0].command_id, "prompt-2");
}

#[test]
fn a_rejected_queued_prompt_records_its_acceptance_ordinal_without_a_turn_start() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::from("go")],
            },
            created_at_ms: 10,
        },
    );
    let accepted = session.applied_event_ordinal;

    apply_observation(
        &mut session,
        RelayObservation::CommandRejected {
            reason: None,
            command_id: "prompt-1".into(),
            command: RelayCommandKind::Prompt,
            message: "transport failed".into(),
        },
    );

    assert!(session.active_turn.is_none());
    assert!(session.queued_prompts.is_empty());
    let outcome = session.last_turn_outcome.clone().expect("an outcome");
    assert_eq!(outcome.accepted_ordinal, Some(accepted));
    assert_eq!(
        outcome.turn_start_position, None,
        "a prompt that never started has no turn in the transcript"
    );
    assert_eq!(
        outcome.outcome,
        TurnOutcomeKind::Rejected {
            reason: None,
            message: "transport failed".into()
        }
    );
}

#[test]
fn queued_prompts_keep_their_own_acceptance_ordinals_through_their_turns() {
    let mut session = MaterializedSession::empty("session");
    let mut accepted = Vec::new();
    for command_id in ["prompt-a", "prompt-b"] {
        apply_observation(
            &mut session,
            RelayObservation::CommandQueued {
                command_id: command_id.into(),
                command: RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::schema::v1::ContentBlock::from("go")],
                },
                created_at_ms: 10,
            },
        );
        accepted.push(session.applied_event_ordinal);
    }
    // The second prompt is accepted before the first one starts, which is
    // exactly the ordering that makes "newest turn" the wrong answer.
    assert!(accepted[1] > accepted[0]);

    for (index, command_id) in ["prompt-a", "prompt-b"].into_iter().enumerate() {
        apply_observation(
            &mut session,
            RelayObservation::CommandStarted {
                command_id: command_id.into(),
                started_at_ms: 20,
            },
        );
        apply_observation(
            &mut session,
            RelayObservation::CommandCompleted {
                barrier_command_id: None,
                command: None,
                command_id: command_id.into(),
                outcome: RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "EndTurn".into(),
                    usage: None,
                },
            },
        );
        assert_eq!(
            session
                .last_turn_outcome
                .as_ref()
                .and_then(|outcome| outcome.accepted_ordinal),
            Some(accepted[index]),
            "{command_id} must report the ordinal its own submission returned"
        );
    }
}

// Hard-won: 1389c545: autonomous Claude turns were treated as idle, allowing mid-turn checkpoints and losing recovery work.
#[test]
fn a_harness_turn_runs_the_session_and_marks_where_it_began() {
    let mut session = MaterializedSession::empty("session");

    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted {
            started_at_ms: 4_200,
        },
    );

    assert_eq!(
        session.execution,
        MaterializedExecutionState::Running {
            started_at_ms: 4_200
        }
    );
    let marker = session.transcript.last().expect("a marker item");
    assert_eq!(
        marker.stable_id,
        format!("{}1", crate::transcript::HARNESS_TURN_ITEM_PREFIX)
    );
    assert!(marker.is_turn_start());
    assert!(matches!(
        &marker.body,
        TranscriptBody::System { text } if text == crate::transcript::HARNESS_TURN_TEXT
    ));

    apply_observation(
        &mut session,
        agent_chunk("picking this back up", "answer-1"),
    );
    assert!(
        session.transcript.iter().any(
            |item| matches!(&item.body, TranscriptBody::Agent { streaming, .. } if *streaming)
        ),
        "output inside the turn streams into a fresh item"
    );

    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnSettled {
            origin: Some("task-notification".into()),
            prompt_in_flight: false,
        },
    );

    assert_eq!(session.execution, MaterializedExecutionState::Idle);
    assert!(
        !session.transcript.iter().any(|item| matches!(
            &item.body,
            TranscriptBody::Agent { streaming, .. } if *streaming
        )),
        "settling closes the streams a canonical export refuses to hold open"
    );
    assert_eq!(
        mj_core::state::latest_completed_turn_ordinal(&session),
        Some(1),
        "the finished turn is covered from the marker that began it"
    );
    assert_eq!(
        mj_core::state::ProjectionWindow::of(&session).latest_turn_start_position,
        Some(1)
    );
}

// Hard-won: db2e99a5: a native turn settling beneath an accepted prompt incorrectly made the session idle.
#[test]
fn a_turn_that_settles_under_an_in_flight_prompt_keeps_the_session_running() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted {
            started_at_ms: 4_200,
        },
    );
    // A prompt typed mid-turn dispatches at once, so it is still running
    // when the harness reaches the boundary of the turn it started.
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::from("go")],
            },
            created_at_ms: 10,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "prompt-1".into(),
            started_at_ms: 20,
        },
    );
    apply_observation(&mut session, agent_chunk("still writing", "answer-1"));

    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnSettled {
            origin: Some("task-notification".into()),
            prompt_in_flight: true,
        },
    );

    assert!(
        matches!(
            session.execution,
            MaterializedExecutionState::Running { .. }
        ),
        "the prompt is still running, so the session is not idle"
    );
    assert!(
        session.transcript.iter().any(
            |item| matches!(&item.body, TranscriptBody::Agent { streaming, .. } if *streaming)
        ),
        "the prompt's own answer keeps streaming into its item"
    );

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "prompt-1".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        },
    );

    assert_eq!(session.execution, MaterializedExecutionState::Idle);
    assert!(
        !session.transcript.iter().any(
            |item| matches!(&item.body, TranscriptBody::Agent { streaming, .. } if *streaming)
        )
    );
}

#[test]
fn finishing_an_acp_prompt_preserves_a_later_native_goal_stream() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted {
            started_at_ms: 4200,
        },
    );
    apply_observation(&mut session, RelayObservation::SessionUpdate { update: Box::new(serde_json::from_value(serde_json::json!({"sessionUpdate":"session_info_update","_meta":{"goal":{"objective":"finish","status":"active"},"execution":{"version":1,"status":"running","turnId":"later"}}})).unwrap()) });
    apply_observation(&mut session, agent_chunk("autonomous work", "later-answer"));
    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "initial-prompt".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        },
    );
    assert!(matches!(
        session.execution,
        MaterializedExecutionState::Running { .. }
    ));
    assert!(session.transcript.iter().any(|item| matches!(
        &item.body,
        TranscriptBody::Agent {
            streaming: true,
            ..
        }
    )));
    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnSettled {
            origin: Some("codex".into()),
            prompt_in_flight: false,
        },
    );
    assert_eq!(session.execution, MaterializedExecutionState::Idle);
}

// Hard-won: 1389c545: worker restart left an open autonomous turn that canonical checkpoint export rejected.
#[test]
fn a_restart_during_a_harness_turn_leaves_an_idle_session_with_no_open_streams() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted {
            started_at_ms: 4_200,
        },
    );
    apply_observation(&mut session, agent_chunk("half a sentence", "answer-1"));

    apply_observation(&mut session, RelayObservation::SessionRestarted);

    assert_eq!(session.unread_interruptions_after(0), 1);
    assert_eq!(
        session.unread_interruptions_after(session.applied_event_ordinal),
        0
    );

    assert_eq!(session.execution, MaterializedExecutionState::Idle);
    assert!(!session.transcript.iter().any(|item| matches!(
        &item.body,
        TranscriptBody::Agent { streaming, .. } | TranscriptBody::Thought { streaming, .. }
            if *streaming
    )));
    canonical_session_from_materialized(&session)
        .expect("a restarted session exports without open streams");
}

// Hard-won: 1389c545: a plan from a self-started turn overwrote the previous turn's plan.
#[test]
fn a_plan_from_a_harness_turn_does_not_overwrite_the_previous_turns_plan() {
    let plan = |content: &str| RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::Plan(
            agent_client_protocol::schema::v1::Plan::new(vec![
                agent_client_protocol::schema::v1::PlanEntry::new(
                    content,
                    agent_client_protocol::schema::v1::PlanEntryPriority::High,
                    agent_client_protocol::schema::v1::PlanEntryStatus::InProgress,
                ),
            ]),
        )),
    };
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::schema::v1::ContentBlock::from("go")],
            },
            created_at_ms: 10,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "prompt-1".into(),
            started_at_ms: 20,
        },
    );
    apply_observation(&mut session, plan("first turn plan"));
    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "prompt-1".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        },
    );

    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted { started_at_ms: 30 },
    );
    apply_observation(&mut session, plan("second turn plan"));

    let plans: Vec<&TranscriptItem> = session
        .transcript
        .iter()
        .filter(|item| matches!(item.body, TranscriptBody::Plan { .. }))
        .map(std::convert::AsRef::as_ref)
        .collect();
    assert_eq!(
        plans.len(),
        2,
        "the self-started turn keeps its own plan instead of rewriting the last one"
    );
}

#[test]
fn interrupted_prompt_before_restart_produces_one_unread_interruption() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("go")],
            },
            created_at_ms: 10,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "prompt".into(),
            started_at_ms: 20,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandInterrupted {
            reason: None,
            command_id: "prompt".into(),
            command: mj_core::relay::RelayCommandKind::Prompt,
            message: "worker restarted".into(),
        },
    );
    let interrupted_at = session.applied_event_ordinal;
    apply_observation(&mut session, RelayObservation::SessionRestarted);
    assert_eq!(session.interruption_event_ordinals(), vec![interrupted_at]);
    assert_eq!(session.unread_interruptions_after(interrupted_at), 0);
    apply_observation(&mut session, RelayObservation::SessionRestarted);
    assert_eq!(session.interruption_event_ordinals(), vec![interrupted_at]);
}

/// I1-17: a turn the user cancelled ends with an "Interrupted" row, so a
/// reader can tell a cut-off reply from a finished one. A finished turn gets
/// no such row.
// Hard-won: a1dfcb6b: cancelled prompt output looked complete because the transcript had no Interrupted row.
#[test]
fn a_cancelled_turn_ends_with_an_interrupted_row() {
    for (stop_reason, marked) in [("cancelled", true), ("end_turn", false)] {
        let mut session = MaterializedSession::empty("session");
        apply_observation(
            &mut session,
            RelayObservation::CommandQueued {
                command_id: "prompt".into(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::from("write a story")],
                },
                created_at_ms: 10,
            },
        );
        apply_observation(
            &mut session,
            RelayObservation::CommandStarted {
                command_id: "prompt".into(),
                started_at_ms: 20,
            },
        );
        apply_observation(
            &mut session,
            RelayObservation::CommandCompleted {
                barrier_command_id: None,
                command: None,
                command_id: "prompt".into(),
                outcome: RelayCommandOutcome::Prompt {
                    stop_reason: stop_reason.into(),
                    usage: None,
                    diagnostic: None,
                },
            },
        );
        let last = session.transcript.last().expect("a transcript row");
        let is_marker = matches!(
            &last.body,
            TranscriptBody::System { text } if text == crate::transcript::TURN_INTERRUPTED_TEXT
        );
        assert_eq!(is_marker, marked, "{stop_reason}: {:?}", session.transcript);
    }
}

/// I2-8: Esc on a turn the harness started on its own (a goal continuation)
/// cancelled it but left no "Interrupted" row and no cancelled outcome, which
/// an ordinary cancelled turn gets. A stop with nothing running, or one that
/// lands on a prompt of ours, adds neither: the prompt's own completion does.
// Hard-won: c8e52abc: cancelling an autonomous harness turn omitted its Interrupted row and cancelled outcome.
#[test]
fn a_cancel_of_a_turn_the_harness_started_ends_it_as_interrupted() {
    let cancel = |session: &mut MaterializedSession, command_id: &str| {
        apply_observation(
            session,
            RelayObservation::CommandQueued {
                command_id: command_id.into(),
                command: RelayCommand::CancelTurn,
                created_at_ms: 40,
            },
        );
        apply_observation(
            session,
            RelayObservation::CommandCompleted {
                barrier_command_id: None,
                command: Some(mj_core::relay::RelayCommandKind::CancelTurn),
                command_id: command_id.into(),
                outcome: RelayCommandOutcome::Cancelled,
            },
        );
    };
    let interrupted_rows = |session: &MaterializedSession| {
        session
            .transcript
            .iter()
            .filter(|item| {
                matches!(
                    &item.body,
                    TranscriptBody::System { text }
                        if text == crate::transcript::TURN_INTERRUPTED_TEXT
                )
            })
            .count()
    };

    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted { started_at_ms: 30 },
    );
    let started_at = session
        .transcript
        .iter()
        .find(|item| {
            item.stable_id
                .starts_with(crate::transcript::HARNESS_TURN_ITEM_PREFIX)
        })
        .expect("the harness turn marker")
        .position;
    cancel(&mut session, "cancel-1");
    assert_eq!(interrupted_rows(&session), 1, "{:?}", session.transcript);
    let outcome = session.last_turn_outcome.as_ref().expect("a turn outcome");
    assert_eq!(
        outcome.result().kind,
        mj_core::event_outcome::TurnResultKind::Cancelled
    );
    assert_eq!(outcome.turn_start_position, Some(started_at));
    assert_eq!(
        outcome.command_id,
        mj_core::continuation::harness_turn_id(started_at)
    );

    // Nothing running: nothing to interrupt.
    let mut idle = MaterializedSession::empty("session");
    cancel(&mut idle, "cancel-idle");
    assert_eq!(interrupted_rows(&idle), 0);
    assert!(idle.last_turn_outcome.is_none());

    // A prompt of ours is the running turn: its completion writes the row.
    let mut prompted = MaterializedSession::empty("session");
    apply_observation(
        &mut prompted,
        RelayObservation::CommandQueued {
            command_id: "prompt".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("write a story")],
            },
            created_at_ms: 10,
        },
    );
    apply_observation(
        &mut prompted,
        RelayObservation::CommandStarted {
            command_id: "prompt".into(),
            started_at_ms: 20,
        },
    );
    cancel(&mut prompted, "cancel-prompt");
    assert_eq!(interrupted_rows(&prompted), 0);
    assert!(prompted.last_turn_outcome.is_none());
}

/// An agent message chunk with no `message_id`, as Grok Build's goal mode streams them.
fn untagged_agent_chunk(text: &str) -> RelayObservation {
    RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            )),
        )),
    }
}

/// An agent thought chunk with no `message_id`, mirroring [`untagged_agent_chunk`].
fn untagged_thought_chunk(text: &str) -> RelayObservation {
    RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::AgentThoughtChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            )),
        )),
    }
}

#[test]
fn streamed_chunks_are_one_unread_logical_agent_message() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("hel"),
                ))
                .message_id("answer-1"),
            )),
        },
    );
    assert_eq!(session.transcript[0].latest_content_event_ordinal, Some(1));
    assert_eq!(session.unread_agent_messages_after(0), 1);
    assert_eq!(session.unread_agent_messages_after(1), 0);

    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("lo"),
                ))
                .message_id("answer-1"),
            )),
        },
    );
    assert_eq!(session.unread_agent_messages_after(0), 1);
    assert_eq!(session.unread_agent_messages_after(1), 1);
    assert!(matches!(
        &session.transcript[0].body,
        TranscriptBody::Agent { chunks, .. }
            if crate::transcript::materialized_chunks_text(chunks) == "hello"
    ));
    assert_eq!(session.transcript[0].position, 1);
    assert_eq!(session.transcript[0].latest_content_event_ordinal, Some(2));

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "prompt-1".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        },
    );
    assert_eq!(session.transcript[0].latest_content_event_ordinal, Some(2));
    assert_eq!(session.unread_agent_messages_after(2), 0);
}

// Hard-won: 89ee2de3: late ACP agent chunks reopened a stream that could never close before checkpoint export.
#[test]
fn agent_chunk_while_idle_is_recorded_closed() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Idle;
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("trailing"),
                ))
                .message_id("msg-1"),
            )),
        },
    );
    let item = session
        .transcript
        .iter()
        .find(|item| item.stable_id == "agent:msg-1")
        .expect("trailing chunk recorded");
    assert!(matches!(
        &item.body,
        TranscriptBody::Agent { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks) == "trailing"
    ));
}

// Hard-won: 89ee2de3: late ACP thought chunks reopened a stream that could never close before checkpoint export.
#[test]
fn thought_chunk_while_idle_is_recorded_closed() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Idle;
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentThoughtChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("late thought"),
                ))
                .message_id("msg-1"),
            )),
        },
    );
    let item = session
        .transcript
        .iter()
        .find(|item| item.stable_id == "thought:msg-1")
        .expect("trailing thought recorded");
    assert!(matches!(
        &item.body,
        TranscriptBody::Thought { streaming, .. } if !*streaming
    ));
}

// Hard-won: 94d3f379: real Grok goal output without message IDs produced thousands of one-token transcript bubbles.
#[test]
fn idle_untagged_agent_chunks_coalesce_into_one_closed_item() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Idle;
    for word in ["Grok ", "streams ", "one ", "word ", "at ", "a ", "time"] {
        apply_observation(&mut session, untagged_agent_chunk(word));
    }
    assert_eq!(session.transcript.len(), 1);
    let item = &session.transcript[0];
    assert!(matches!(
        &item.body,
        TranscriptBody::Agent { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks)
                    == "Grok streams one word at a time"
    ));
}

// Hard-won: 94d3f379: real Grok goal output without message IDs produced thousands of one-token transcript bubbles.
#[test]
fn idle_untagged_thought_chunks_coalesce_into_one_closed_item() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Idle;
    for word in ["thinking ", "in ", "small ", "pieces"] {
        apply_observation(&mut session, untagged_thought_chunk(word));
    }
    assert_eq!(session.transcript.len(), 1);
    let item = &session.transcript[0];
    assert!(matches!(
        &item.body,
        TranscriptBody::Thought { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks)
                    == "thinking in small pieces"
    ));
}

#[test]
fn backward_relay_clock_never_regresses_transcript_change_times() {
    let mut session = MaterializedSession::empty("session-1");
    let mut first = event(
        &session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("first"),
                ))
                .message_id("answer-1"),
            )),
        },
    );
    first.recorded_at_ms = 1_000;
    first.digest = relay_event_digest(&first).unwrap();
    apply(&mut session, first);

    let mut backward = event(
        &session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new(" second"),
                ))
                .message_id("answer-1"),
            )),
        },
    );
    backward.recorded_at_ms = 500;
    backward.digest = relay_event_digest(&backward).unwrap();
    apply(&mut session, backward);
    assert_eq!(session.transcript[0].last_changed_at_ms, 1_000);

    let mut completion = event(
        &session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "prompt-1".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        },
    );
    completion.recorded_at_ms = 250;
    completion.digest = relay_event_digest(&completion).unwrap();
    apply(&mut session, completion);
    assert_eq!(session.transcript[0].last_changed_at_ms, 1_000);
    assert_eq!(session.last_activity_at_ms(), Some(1_000));
}

#[test]
fn tool_update_without_an_initial_call_is_ignored_and_advances_the_frontier() {
    let mut session = MaterializedSession::empty("session-1");
    let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "missing-tool",
        ToolCallUpdateFields::new().title("updated"),
    ));
    let relay_event = event(
        &session,
        RelayObservation::SessionUpdate {
            update: Box::new(update),
        },
    );

    let projected = project_relay_event(&session, &relay_event)
        .expect("a delayed pre-resume tool update is an observable no-op");
    apply_committed_projection_event(&mut session, &relay_event, projected.mutation)
        .expect("the no-op still advances the committed relay frontier");

    assert!(session.transcript.is_empty());
    assert_eq!(session.applied_event_ordinal, 1);
}

#[test]
fn metadata_only_tool_update_without_an_initial_call_is_ignored() {
    let mut session = MaterializedSession::empty("session-1");
    let update = SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new("pre-resume-tool", ToolCallUpdateFields::new()).meta(
            serde_json::Map::from_iter([(
                "terminal_output_delta".into(),
                json!({"data": "late output"}),
            )]),
        ),
    );
    let relay_event = event(
        &session,
        RelayObservation::SessionUpdate {
            update: Box::new(update),
        },
    );

    let projected = project_relay_event(&session, &relay_event)
        .expect("private metadata cannot change the transcript projection");
    apply_committed_projection_event(&mut session, &relay_event, projected.mutation)
        .expect("the no-op still advances the committed relay frontier");

    assert!(session.transcript.is_empty());
    assert_eq!(session.applied_event_ordinal, 1);
}

// Hard-won: 111f6dcd: Codex resent a revised tool call as a new item, changing identity and failing every recovery retry.
#[test]
fn resent_tool_call_keeps_identity_and_replaces_the_call_payload() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                "call-1",
                "read file",
            ))),
        },
    );
    let created = TranscriptItem::clone(&session.transcript[0]);
    assert_eq!(created.position, 1);
    assert_eq!(created.created_at_ms, 100);

    let resend = event(
        &session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(
                ToolCall::new("call-1", "read file again")
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed),
            )),
        },
    );
    let projected = project_relay_event(&session, &resend).unwrap();
    let TranscriptMutation::Upsert(item) = projected
            .mutation
            .transcript
            .iter()
            .find(|mutation| {
                matches!(mutation, TranscriptMutation::Upsert(item) if item.stable_id == "tool:call-1")
            })
            .expect("the re-sent tool call upserts its existing item")
            .clone()
        else {
            unreachable!("matched an upsert above");
        };
    assert_eq!(item.position, created.position);
    assert_eq!(item.created_at_ms, created.created_at_ms);
    assert_eq!(item.last_changed_at_ms, resend.recorded_at_ms);
    assert_eq!(
        item.latest_content_event_ordinal,
        created.latest_content_event_ordinal
    );
    let TranscriptBody::Tool { call, .. } = &item.body else {
        panic!("re-sent tool call stayed a tool item");
    };
    assert_eq!(call["title"], json!("read file again"));

    apply_committed_projection_event(&mut session, &resend, projected.mutation)
        .expect("the merged item passes the projection integrity checks");
    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].position, created.position);
}

// Hard-won: 111f6dcd: a revised full call after an incremental update created a second transcript item.
#[test]
fn tool_call_update_then_resent_tool_call_survives_the_projection() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new("call-1", "shell"))),
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed),
            ))),
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                "call-1",
                "shell (retried)",
            ))),
        },
    );

    assert_eq!(session.transcript.len(), 1);
    let item = &session.transcript[0];
    assert_eq!(item.position, 1);
    assert_eq!(item.created_at_ms, 100);
    assert_eq!(item.last_changed_at_ms, 300);
    let TranscriptBody::Tool { call, .. } = &item.body else {
        panic!("the item stayed a tool item");
    };
    assert_eq!(call["title"], json!("shell (retried)"));
}

/// A tool call whose only content is a terminal reference, the shape
/// kimi-code sends for every Bash call.
fn terminal_tool_call(call_id: &'static str, terminal_id: &'static str) -> RelayObservation {
    RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::ToolCall(
            ToolCall::new(call_id, "shell").content(vec![ToolCallContent::Terminal(
                agent_client_protocol::schema::v1::Terminal::new(terminal_id),
            )]),
        )),
    }
}

fn terminal_output(terminal_id: &str) -> RelayObservation {
    RelayObservation::TerminalOutput {
        terminal_id: terminal_id.into(),
        output: "build finished\n".into(),
        truncated: false,
        exit_code: Some(0),
        signal: None,
    }
}

fn fallback_terminal_tool(terminal_id: &str, command: &str) -> RelayObservation {
    RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::ToolCall(
            mj_core::acp::fallback_terminal_tool_call(terminal_id, command.into()),
        )),
    }
}

fn attached_terminal_outputs(item: &TranscriptItem) -> &[TerminalOutputRecord] {
    let TranscriptBody::Tool {
        terminal_outputs, ..
    } = &item.body
    else {
        panic!("expected a tool item, got {:?}", item.body);
    };
    terminal_outputs
}

#[test]
fn real_tool_call_replaces_fallback_and_keeps_its_start_order() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, fallback_terminal_tool("term-1", "cargo test"));
    apply_observation(&mut session, terminal_tool_call("call-1", "term-1"));
    apply_observation(&mut session, terminal_output("term-1"));

    assert_eq!(session.transcript.len(), 1, "the fallback was consumed");
    let item = &session.transcript[0];
    assert_eq!(item.stable_id, "tool:call-1");
    assert_eq!(item.position, 1);
    assert_eq!(attached_terminal_outputs(item).len(), 1);
}

#[test]
fn fallback_is_suppressed_when_real_tool_already_claims_terminal() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, terminal_tool_call("call-1", "term-1"));
    apply_observation(&mut session, fallback_terminal_tool("term-1", "cargo test"));
    apply_observation(&mut session, terminal_output("term-1"));

    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].stable_id, "tool:call-1");
    assert_eq!(attached_terminal_outputs(&session.transcript[0]).len(), 1);
}

fn kimi_raw_tool_update(call_id: &'static str, output: &'static str) -> RelayObservation {
    RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .raw_output(json!({
                    "type": "Bash",
                    "output": output.as_bytes(),
                    "exit_code": 0,
                    "command": "cargo test"
                })),
        ))),
    }
}

#[test]
fn raw_result_before_terminal_close_claims_the_fallback() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, fallback_terminal_tool("term-1", "cargo test"));
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                "call-1",
                "Execute `cargo test`",
            ))),
        },
    );
    apply_observation(
        &mut session,
        kimi_raw_tool_update("call-1", "build finished\n"),
    );
    apply_observation(&mut session, terminal_output("term-1"));

    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].stable_id, "tool:call-1");
    assert_eq!(session.transcript[0].position, 2);
    assert_eq!(attached_terminal_outputs(&session.transcript[0]).len(), 1);
}

#[test]
fn raw_result_after_terminal_close_claims_the_fallback() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, fallback_terminal_tool("term-1", "cargo test"));
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                "call-1",
                "Execute `cargo test`",
            ))),
        },
    );
    apply_observation(&mut session, terminal_output("term-1"));
    apply_observation(
        &mut session,
        kimi_raw_tool_update("call-1", "build finished\n"),
    );

    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].stable_id, "tool:call-1");
    assert_eq!(session.transcript[0].position, 2);
    assert_eq!(attached_terminal_outputs(&session.transcript[0]).len(), 1);
}

#[test]
fn late_fallback_claims_output_from_a_fast_terminal() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, terminal_output("term-1"));
    apply_observation(&mut session, fallback_terminal_tool("term-1", "true"));

    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].stable_id, "tool:hel-terminal:term-1");
    let TranscriptBody::Tool { call, .. } = &session.transcript[0].body else {
        panic!("the parked output became a fallback tool");
    };
    let call: ToolCall = serde_json::from_value(call.clone()).unwrap();
    assert_eq!(call.status, ToolCallStatus::Completed);
}

#[test]
fn indexed_page_projection_tracks_terminal_and_tool_replacements() {
    let mut session = MaterializedSession::empty("session-1");
    let mut index = ProjectionIndex::new(&session);
    apply_indexed_observation(&mut session, &mut index, terminal_output("term-1"));
    apply_indexed_observation(
        &mut session,
        &mut index,
        terminal_tool_call("call-1", "term-1"),
    );
    apply_indexed_observation(&mut session, &mut index, terminal_output("term-1"));

    assert_eq!(session.transcript.len(), 1, "parked output was consumed");
    assert_eq!(session.transcript[0].stable_id, "tool:call-1");
    let outputs = attached_terminal_outputs(&session.transcript[0]);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].terminal_id, "term-1");
}

#[test]
fn terminal_output_before_the_tool_call_attaches_when_the_call_arrives() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, terminal_output("term-1"));

    // Output nobody refers to yet is parked in its own item rather than
    // dropped, so a terminal a call never names still reaches the reader.
    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].stable_id, "terminal:term-1");
    assert!(matches!(
        &session.transcript[0].body,
        TranscriptBody::TerminalOutput { record } if record.terminal_id == "term-1"
    ));

    apply_observation(&mut session, terminal_tool_call("call-1", "term-1"));

    assert_eq!(
        session.transcript.len(),
        1,
        "the tool call consumes the parked item: {:?}",
        session.transcript
    );
    assert_eq!(session.transcript[0].stable_id, "tool:call-1");
    let outputs = attached_terminal_outputs(&session.transcript[0]);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].output, "build finished\n");

    // Both orderings converge on the same tool body.
    let mut reversed = MaterializedSession::empty("session-1");
    apply_observation(&mut reversed, terminal_tool_call("call-1", "term-1"));
    apply_observation(&mut reversed, terminal_output("term-1"));
    assert_eq!(
        attached_terminal_outputs(&reversed.transcript[0]),
        outputs,
        "output arriving before or after the call must read the same"
    );
    assert_eq!(reversed.transcript[0].last_changed_at_ms, 200);
}

#[test]
fn mismatched_raw_result_does_not_hide_a_genuine_orphan_failure() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new("call-1", "Execute"))),
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::TerminalOutput {
            terminal_id: "term-1".into(),
            output: "orphan failure\n".into(),
            truncated: false,
            exit_code: Some(1),
            signal: None,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                    .raw_output(json!({
                        "output": b"different output",
                        "exit_code": 1
                    })),
            ))),
        },
    );

    assert_eq!(session.transcript.len(), 2);
    assert!(session.transcript.iter().any(|item| matches!(
        &item.body,
        TranscriptBody::TerminalOutput { record }
            if record.output == "orphan failure\n"
    )));
}

#[test]
fn identical_orphan_results_are_not_assigned_arbitrarily() {
    const OUTPUT: &str = "same output\n";
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new("call-1", "Execute"))),
        },
    );
    for terminal_id in ["term-1", "term-2"] {
        apply_observation(
            &mut session,
            RelayObservation::TerminalOutput {
                terminal_id: terminal_id.into(),
                output: OUTPUT.into(),
                truncated: false,
                exit_code: Some(1),
                signal: None,
            },
        );
    }
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                    .raw_output(json!({
                        "output": OUTPUT.as_bytes(),
                        "exit_code": 1
                    })),
            ))),
        },
    );

    assert_eq!(
        session
            .transcript
            .iter()
            .filter(|item| matches!(item.body, TranscriptBody::TerminalOutput { .. }))
            .count(),
        2,
        "identical concurrent results need an explicit reference"
    );
    assert!(attached_terminal_outputs(&session.transcript[0]).is_empty());
}

#[test]
fn wholesale_tool_call_update_keeps_the_attached_terminal_output() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, terminal_tool_call("call-1", "term-1"));
    apply_observation(&mut session, terminal_output("term-1"));
    // `ToolCall::update` replaces `content` wholesale, which is why the
    // output lives beside the call rather than inside it.
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Terminal(
                        agent_client_protocol::schema::v1::Terminal::new("term-1"),
                    )]),
            ))),
        },
    );

    assert_eq!(session.transcript.len(), 1);
    let outputs = attached_terminal_outputs(&session.transcript[0]);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].output, "build finished\n");
    let TranscriptBody::Tool { call, .. } = &session.transcript[0].body else {
        panic!("the item stayed a tool item");
    };
    assert_eq!(call["status"], json!("completed"));
}

/// Grok Build names the terminal on a mid-flight update and then replaces
/// `content` wholesale with plain text before the terminal is reaped, so
/// the close event arrives with nothing in the call pointing at it.
// Hard-won: b364ed85: live Grok updates dropped a prior terminal reference before its output arrived.
#[test]
fn a_tool_call_that_dropped_its_terminal_reference_still_attaches_the_output() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, terminal_tool_call("call-1", "term-1"));
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new("ran the build"),
                    ))]),
            ))),
        },
    );
    apply_observation(&mut session, terminal_output("term-1"));

    assert_eq!(
        session.transcript.len(),
        1,
        "the output attaches instead of parking in its own item: {:?}",
        session.transcript
    );
    assert_eq!(session.transcript[0].stable_id, "tool:call-1");
    let outputs = attached_terminal_outputs(&session.transcript[0]);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].output, "build finished\n");
    let TranscriptBody::Tool {
        call,
        terminal_refs,
        ..
    } = &session.transcript[0].body
    else {
        panic!("the item stayed a tool item");
    };
    assert_eq!(terminal_refs, &["term-1".to_owned()]);
    assert_eq!(
        tool_call_terminal_ids(call),
        Vec::<String>::new(),
        "the final call really did drop the reference"
    );
}

#[test]
fn session_info_update_caps_large_titles_in_the_published_projection() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new()
                    .title("word ".repeat(20_000)),
            )),
        },
    );

    let expected = format!("{}word…", "word ".repeat(50));
    assert_eq!(session.session_title.as_deref(), Some(expected.as_str()));
    assert_eq!(session.resolved_title().as_deref(), Some(expected.as_str()));
    assert!(serde_json::to_string(&session).unwrap().len() < 10_000);
}

#[test]
fn session_info_update_without_title_preserves_the_provisional_title() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("first prompt"))],
            },
            created_at_ms: 100,
        },
    );

    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new()
                    .updated_at("2026-08-31T12:00:00Z"),
            )),
        },
    );

    assert_eq!(session.session_title.as_deref(), Some("first prompt"));
}

#[test]
fn explicit_session_title_clear_restores_the_prompt_fallback() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("first prompt"))],
            },
            created_at_ms: 100,
        },
    );

    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new().title(None),
            )),
        },
    );

    assert_eq!(session.session_title, None);
    assert_eq!(session.resolved_title().as_deref(), Some("first prompt"));
}

#[test]
fn queue_changes_project_only_from_their_completion_events() {
    let mut session = MaterializedSession::empty("session-1");
    session.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-1".into(),
        kind: QueuedCommandKind::Prompt,
        content: vec![json!({"type": "text", "text": "later"})],
        queued_at_ms: 10,
    });

    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "remove-1".into(),
            command: RelayCommand::RemoveQueuedPrompt {
                queued_command_id: "queued-1".into(),
            },
            created_at_ms: 100,
        },
    );
    assert_eq!(session.queued_prompts.len(), 1);

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "remove-1".into(),
            outcome: RelayCommandOutcome::QueueChanged {
                removed_command_ids: vec!["queued-1".into()],
            },
        },
    );
    assert!(session.queued_prompts.is_empty());

    session.queued_prompts.extend([
        MaterializedQueuedPrompt {
            accepted_ordinal: None,
            command_id: "queued-2".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![json!({"type": "text", "text": "two"})],
            queued_at_ms: 20,
        },
        MaterializedQueuedPrompt {
            accepted_ordinal: None,
            command_id: "queued-3".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![json!({"type": "text", "text": "three"})],
            queued_at_ms: 30,
        },
    ]);
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "clear-1".into(),
            command: RelayCommand::ClearQueuedPrompts,
            created_at_ms: 200,
        },
    );
    assert_eq!(session.queued_prompts.len(), 2);

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "clear-1".into(),
            outcome: RelayCommandOutcome::QueueChanged {
                removed_command_ids: vec!["queued-2".into(), "queued-3".into()],
            },
        },
    );
    assert!(session.queued_prompts.is_empty());
}

#[test]
fn rejected_close_rolls_closing_projection_back_to_idle() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "close-1".into(),
            command: RelayCommand::Close {
                barrier_command_id: "barrier-1".into(),
                expected: mj_core::relay::RelayCursor {
                    ordinal: 0,
                    digest: "0".repeat(64),
                },
            },
            created_at_ms: 100,
        },
    );
    assert_eq!(session.execution, MaterializedExecutionState::Closing);

    apply_observation(
        &mut session,
        RelayObservation::CommandRejected {
            reason: None,
            command_id: "close-1".into(),
            command: RelayCommandKind::Close,
            message: "ACP close failed".into(),
        },
    );
    assert_eq!(session.execution, MaterializedExecutionState::Idle);
}

// Hard-won: 0d9df1dd: rejected command notices exposed internal command IDs in the conversation.
#[test]
fn a_rejected_command_notice_does_not_show_the_command_id() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandRejected {
            reason: None,
            command_id: "set-config-0123abcd".into(),
            command: RelayCommandKind::SetConfig,
            message: "\"gpt-9\" is not an available model value".into(),
        },
    );
    let notices = session
        .transcript
        .iter()
        .filter_map(|item| match &item.body {
            TranscriptBody::System { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices, ["\"gpt-9\" is not an available model value"]);
}

#[test]
fn control_command_outcomes_do_not_end_an_active_prompt() {
    let mut session = MaterializedSession::empty("session-1");
    session.applied_event_ordinal = 2;
    session.applied_event_digest = "a".repeat(64);
    session.execution = MaterializedExecutionState::Running { started_at_ms: 100 };
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "agent:answer-1".into(),
        position: 2,
        latest_content_event_ordinal: Some(2),
        created_at_ms: 200,
        last_changed_at_ms: 200,
        body: TranscriptBody::Agent {
            chunks: vec![json!({
                "content": {"type": "text", "text": "working"}
            })],
            streaming: true,
        },
    }));

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "config-1".into(),
            outcome: RelayCommandOutcome::Configured,
        },
    );
    assert!(matches!(
        session.execution,
        MaterializedExecutionState::Running { .. }
    ));
    assert!(matches!(
        session.transcript[0].body,
        TranscriptBody::Agent {
            streaming: true,
            ..
        }
    ));

    apply_observation(
        &mut session,
        RelayObservation::CommandRejected {
            reason: None,
            command_id: "cancel-1".into(),
            command: RelayCommandKind::Cancel,
            message: "not cancellable".into(),
        },
    );
    assert!(matches!(
        session.execution,
        MaterializedExecutionState::Running { .. }
    ));
    assert!(matches!(
        session.transcript[0].body,
        TranscriptBody::Agent {
            streaming: true,
            ..
        }
    ));
}

fn agent_chunk(text: &str, message_id: &str) -> RelayObservation {
    RelayObservation::SessionUpdate {
        update: Box::new(SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))
            .message_id(message_id),
        )),
    }
}

// Hard-won: ff088d42: a real 15,000-chunk DeepSeek thought caused quadratic projection work and multi-gigabyte RSS.
#[test]
fn streamed_text_for_one_message_id_becomes_a_single_chunk() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    for token in ["think", "ing ", "it ", "through"] {
        apply_observation(
            &mut session,
            RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AgentThoughtChunk(
                    agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                        TextContent::new(token),
                    ))
                    .message_id("msg-1"),
                )),
            },
        );
    }
    for token in ["here ", "is ", "the ", "answer"] {
        apply_observation(
            &mut session,
            RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AgentMessageChunk(
                    agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                        TextContent::new(token),
                    ))
                    .message_id("msg-1"),
                )),
            },
        );
    }

    let thought = session
        .transcript
        .iter()
        .find(|item| item.stable_id == "thought:msg-1")
        .expect("thought recorded");
    let TranscriptBody::Thought { chunks, .. } = &thought.body else {
        panic!("expected a thought body: {:?}", thought.body);
    };
    assert_eq!(chunks.len(), 1, "chunks: {chunks:?}");
    assert_eq!(
        crate::transcript::materialized_chunks_text(chunks),
        "thinking it through"
    );

    let agent = session
        .transcript
        .iter()
        .find(|item| item.stable_id == "agent:msg-1")
        .expect("agent message recorded");
    let TranscriptBody::Agent { chunks, .. } = &agent.body else {
        panic!("expected an agent body: {:?}", agent.body);
    };
    assert_eq!(chunks.len(), 1, "chunks: {chunks:?}");
    assert_eq!(
        crate::transcript::materialized_chunks_text(chunks),
        "here is the answer"
    );
}

#[test]
fn clear_preserves_history_and_adds_one_durable_context_boundary() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(
        &mut session,
        RelayObservation::Warning {
            message: "earlier history".into(),
        },
    );
    let previous = session.transcript[0].clone();
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "clear-request".into(),
            command: RelayCommand::ClearContext,
            created_at_ms: 200,
        },
    );
    assert!(matches!(
        session.execution,
        MaterializedExecutionState::Running { .. }
    ));
    let complete = event(
        &session,
        RelayObservation::CommandCompleted {
            barrier_command_id: None,
            command: None,
            command_id: "clear-request".into(),
            outcome: RelayCommandOutcome::ContextCleared {
                native_session_id: "new".into(),
                memory: None,
            },
        },
    );
    apply(&mut session, complete.clone());
    assert_eq!(session.execution, MaterializedExecutionState::Idle);
    assert_eq!(session.transcript[0], previous);
    assert_eq!(
        session
            .transcript
            .iter()
            .filter(|item| mj_core::archive::is_context_boundary(&item.stable_id))
            .count(),
        1
    );
    assert!(session.last_turn_outcome.is_none());
    // The projection refuses duplicate ordinals; command retry deduplication
    // happens in the durable relay before a second event can be emitted.
    assert!(project_relay_event(&session, &complete).is_err());
    assert_eq!(
        session
            .transcript
            .iter()
            .filter(|item| mj_core::archive::is_context_boundary(&item.stable_id))
            .count(),
        1
    );
}

#[test]
fn windowed_current_turn_keeps_streaming_and_tool_updates_identical_to_full_history() {
    use agent_client_protocol::schema::v1::ToolCallStatus;
    let mut full = MaterializedSession::empty("window-stream");
    // Multiple settled turns, with enough text to exceed a pipe buffer too.
    for turn in 0..12 {
        let command_id = format!("prompt-{turn}");
        apply_observation(
            &mut full,
            RelayObservation::CommandQueued {
                command_id: command_id.clone(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::from("continue")],
                },
                created_at_ms: 0,
            },
        );
        apply_observation(
            &mut full,
            RelayObservation::CommandStarted {
                command_id,
                started_at_ms: 0,
            },
        );
        for _ in 0..100 {
            apply_observation(
                &mut full,
                RelayObservation::Notice {
                    message: "recorded context ".repeat(20),
                },
            );
        }
    }
    apply_observation(
        &mut full,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(
                ToolCall::new("current-tool", "read file").status(ToolCallStatus::InProgress),
            )),
        },
    );
    apply_observation(&mut full, untagged_agent_chunk("first "));
    let mut live = full.clone();
    let mut window = mj_core::state::ProjectionWindow::of(&live);
    window.trim(&mut live, 1024);
    assert!(window.omitted_items > 0);
    assert!(live.transcript.len() < full.transcript.len());
    for observation in [
        untagged_agent_chunk("second"),
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "current-tool",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            ))),
        },
    ] {
        let next = event(&full, observation);
        apply(&mut full, next.clone());
        apply(&mut live, next);
        window.trim(&mut live, 1024);
        for item in &live.transcript {
            let durable = full
                .transcript
                .iter()
                .find(|candidate| candidate.stable_id == item.stable_id)
                .unwrap();
            assert_eq!(item, durable);
        }
        assert_eq!(
            live.transcript.len() + window.omitted_items,
            full.transcript.len()
        );
    }
    let TranscriptBody::Agent { chunks, .. } = &live.transcript.last().unwrap().body else {
        panic!("agent stream")
    };
    assert_eq!(
        crate::transcript::materialized_chunks_text(chunks),
        "first second"
    );
    let tool = live
        .transcript
        .iter()
        .find(|item| item.stable_id == "tool:current-tool")
        .unwrap();
    let TranscriptBody::Tool { call, .. } = &tool.body else {
        panic!("tool")
    };
    assert_eq!(call["status"], "completed");
}

// Hard-won: 32f1541e: checkpoint recovery failures exposed daemon bookkeeping IDs as transcript messages.
#[test]
fn an_interrupted_checkpoint_barrier_adds_nothing_to_the_transcript() {
    let mut session = MaterializedSession::empty("session");
    for command in [
        mj_core::relay::RelayCommandKind::BeginCheckpoint,
        mj_core::relay::RelayCommandKind::ReleaseCheckpoint,
    ] {
        apply_observation(
            &mut session,
            RelayObservation::CommandInterrupted {
                reason: None,
                command_id: "worker-upgrade-0123abcd".into(),
                command,
                message: "relay restarted without the controller that owned the checkpoint barrier"
                    .into(),
            },
        );
    }
    assert!(session.transcript.is_empty(), "{:?}", session.transcript);

    // A person's own command still reports its failure.
    apply_observation(
        &mut session,
        RelayObservation::CommandRejected {
            reason: None,
            command_id: "clear".into(),
            command: mj_core::relay::RelayCommandKind::ClearContext,
            message: "busy".into(),
        },
    );
    assert_eq!(session.transcript.len(), 1);
}

/// Issue 1217: the reminder ran as "Agent continued on its own", so the
/// transcript hid the prompt the child was obeying.
// Hard-won: e97c0266: a child obeyed an invisible handback reminder and stopped before finishing its task.
#[test]
fn a_handback_reminder_is_shown_as_the_prompt_that_started_its_turn() {
    let mut session = MaterializedSession::empty("child");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "handback-reminder-36".into(),
            command: RelayCommand::HandbackReminder {
                completed_command_id: "task".into(),
                completed_ordinal: 36,
            },
            created_at_ms: 10,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "handback-reminder-36".into(),
            started_at_ms: 20,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::HarnessTurnStarted { started_at_ms: 21 },
    );

    let turn = session.active_turn.clone().expect("the reminder's turn");
    assert_eq!(turn.command_id, "handback-reminder-36");
    assert!(session.transcript.iter().any(|item| matches!(
        &item.body,
        TranscriptBody::User { content }
            if crate::transcript::materialized_content_text(content)
                == mj_core::subagent::HANDBACK_REMINDER_TEXT
    )));
    assert!(!session.transcript.iter().any(|item| matches!(
        &item.body,
        TranscriptBody::System { text } if text == crate::transcript::HARNESS_TURN_TEXT
    )));
}

#[test]
fn canonical_round_trip_preserves_cursor_and_logical_positions() {
    let mut session = MaterializedSession::empty("session-1");
    session.applied_event_ordinal = 4;
    session.applied_event_digest = "a".repeat(64);
    session.last_activity_at_ms = Some(40);
    session.session_title = Some("Build it".into());
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "agent:a".into(),
        position: 2,
        latest_content_event_ordinal: Some(4),
        created_at_ms: 20,
        last_changed_at_ms: 40,
        body: TranscriptBody::Agent {
            chunks: vec![json!({
                "content": {"type": "text", "text": "done"},
                "messageId": "a",
                "_meta": {"provider": "test"}
            })],
            streaming: false,
        },
    }));
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "thought:t".into(),
        position: 3,
        latest_content_event_ordinal: None,
        created_at_ms: 30,
        last_changed_at_ms: 30,
        body: TranscriptBody::Thought {
            chunks: vec![json!({
                "content": {
                    "type": "text",
                    "text": "reasoning",
                    "_meta": {"contentProvider": "test"}
                },
                "messageId": "t",
                "_meta": {"chunkProvider": "test"}
            })],
            streaming: false,
        },
    }));
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "tool:call-1".into(),
        position: 4,
        latest_content_event_ordinal: None,
        created_at_ms: 40,
        last_changed_at_ms: 40,
        body: TranscriptBody::Tool {
            call: json!({
                "toolCallId": "call-1",
                "title": "Read file",
                "kind": "read",
                "status": "completed",
                "content": [{"type": "terminal", "terminalId": "term-1"}],
                "rawInput": {"path": "README.md"},
                "rawOutput": {"bytes": 42},
                "_meta": {"provider": "test"}
            }),
            terminal_outputs: vec![TerminalOutputRecord {
                terminal_id: "term-1".into(),
                output: "ok\n".into(),
                truncated: true,
                exit_code: Some(0),
                signal: None,
            }],
            // "term-3" is a reference the call no longer carries, so only
            // the remembered list can survive the archive round trip.
            terminal_refs: vec!["term-1".into(), "term-3".into()],
            presentation: Some(Box::new(crate::transcript::ToolCallPresentation {
                summary: "Read".into(),
                source: "Read file".into(),
                source_kind: crate::transcript::ToolSummarySourceKind::Title,
                tool_kind: agent_client_protocol::schema::v1::ToolKind::Read,
                summary_version: crate::transcript::TOOL_SUMMARY_VERSION,
            })),
        },
    }));
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "terminal:term-2".into(),
        position: 4,
        latest_content_event_ordinal: None,
        created_at_ms: 40,
        last_changed_at_ms: 40,
        body: TranscriptBody::TerminalOutput {
            record: TerminalOutputRecord {
                terminal_id: "term-2".into(),
                output: "orphaned output\n".into(),
                truncated: false,
                exit_code: None,
                signal: Some("SIGKILL".into()),
            },
        },
    }));
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: "plan:4".into(),
        position: 4,
        latest_content_event_ordinal: None,
        created_at_ms: 40,
        last_changed_at_ms: 40,
        body: TranscriptBody::Plan {
            plan: json!({
                "entries": [{
                    "content": "finish",
                    "priority": "high",
                    "status": "in_progress",
                    "_meta": {"entryProvider": "test"}
                }],
                "_meta": {"planProvider": "test"}
            }),
        },
    }));
    session.transcript.push(Arc::new(TranscriptItem {
        stable_id: plan_proposal_item_id(4),
        position: 4,
        latest_content_event_ordinal: None,
        created_at_ms: 40,
        last_changed_at_ms: 40,
        body: TranscriptBody::PlanProposal {
            proposal_id: "plan-review-1".into(),
            plan: "1. Read the code\n2. Change it".into(),
        },
    }));
    session.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-config".into(),
        kind: QueuedCommandKind::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        },
        content: vec![json!({"type": "text", "text": "/model sonnet"})],
        queued_at_ms: 50,
    });
    let canonical = canonical_session_from_materialized(&session).unwrap();
    canonical.validate().unwrap();
    assert_eq!(
        canonical.queued_prompts[0].kind,
        CanonicalQueuedCommandKind::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        }
    );
    let restored = materialized_session_from_canonical("session-1", &canonical).unwrap();
    assert_eq!(restored.applied_event_ordinal, 4);
    assert_eq!(restored.transcript[0].position, 2);
    assert_eq!(restored.unread_agent_messages_after(1), 1);
    assert_eq!(restored, session);
}

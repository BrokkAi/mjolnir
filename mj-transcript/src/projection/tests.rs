use agent_client_protocol::schema::v1::{
    ContentBlock, TextContent, ToolCallUpdate, ToolCallUpdateFields,
};

use super::*;
use mj_core::relay::{
    RelayCommand, RelayCommandOutcome, RelayObservation, UserShellResult, UserShellStatus,
    relay_event_digest,
};
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

#[test]
fn a_completed_prompt_records_its_stop_reason_and_clears_the_running_turn() {
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
    assert_eq!(
        session.queued_prompts[0].accepted_ordinal,
        Some(accepted),
        "a queue entry remembers the ordinal its caller was told"
    );

    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "prompt-1".into(),
            started_at_ms: 20,
        },
    );
    let turn = session.active_turn.clone().expect("a running turn");
    assert_eq!(turn.command_id, "prompt-1");
    assert_eq!(turn.accepted_ordinal, Some(accepted));
    assert_eq!(turn.turn_start_position, session.applied_event_ordinal);
    assert_eq!(turn.started_at_ms, 20);

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
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
    assert_eq!(outcome.accepted_ordinal, Some(accepted));
    assert_eq!(outcome.turn_start_position, Some(turn.turn_start_position));
    assert_eq!(outcome.completed_ordinal, session.applied_event_ordinal);
    assert_eq!(
        outcome.outcome,
        TurnOutcomeKind::Completed {
            stop_reason: "EndTurn".into()
        }
    );
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

#[test]
fn session_restarts_project_as_distinct_durable_system_lines() {
    let mut session = MaterializedSession::empty("session");
    apply_observation(&mut session, RelayObservation::SessionRestarted);
    apply_observation(&mut session, RelayObservation::SessionRestarted);

    assert_eq!(session.transcript.len(), 2);
    assert!(
        session
            .transcript
            .iter()
            .all(|item| item.is_session_restart())
    );
    assert_eq!(session.unread_interruptions_after(0), 0);
    assert!(session.transcript.iter().all(|item| matches!(
        &item.body,
        TranscriptBody::System { text }
            if text == crate::transcript::SESSION_RESTART_TEXT
    )));
    assert_ne!(
        session.transcript[0].stable_id,
        session.transcript[1].stable_id
    );

    let canonical = canonical_session_from_materialized(&session).unwrap();
    let restored = materialized_session_from_canonical("session", &canonical).unwrap();
    assert_eq!(restored.unread_interruptions_after(0), 0);
    assert!(
        restored
            .transcript
            .iter()
            .all(|item| item.is_session_restart())
    );
}

#[test]
fn shell_output_updates_one_durable_transcript_item() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "shell-1".into(),
            command: RelayCommand::RunUserShell {
                command: "cargo test".into(),
            },
            created_at_ms: 100,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "shell-1".into(),
            started_at_ms: 200,
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::UserShellOutput {
            command_id: "shell-1".into(),
            command: "cargo test".into(),
            stdout: "running tests".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        },
    );
    assert_eq!(session.transcript.len(), 1);
    assert!(matches!(
        &session.transcript[0].body,
        TranscriptBody::System { text }
            if text.contains("Shell · running") && text.contains("running tests")
    ));

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            command_id: "shell-1".into(),
            outcome: RelayCommandOutcome::UserShell {
                result: UserShellResult {
                    command: "cargo test".into(),
                    stdout: "all green".into(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    exit_code: Some(0),
                    signal: None,
                    duration_ms: 321,
                    status: UserShellStatus::Exited,
                    error: None,
                },
            },
        },
    );
    assert_eq!(session.transcript.len(), 1);
    assert_eq!(session.transcript[0].stable_id, "shell:shell-1");
    assert!(matches!(
        &session.transcript[0].body,
        TranscriptBody::System { text }
            if text.contains("Shell · done · 321 ms") && text.contains("all green")
    ));
}

#[test]
fn elicitation_projection_keeps_only_pending_request_metadata() {
    let mut session = MaterializedSession::empty("session-1");
    let request = mj_core::elicitation::ElicitationRequest {
        id: "elicitation-1".into(),
        message: "Choose one".into(),
        title: None,
        description: None,
        fields: Vec::new(),
    };
    apply_observation(
        &mut session,
        RelayObservation::ElicitationRequested {
            request: request.clone(),
        },
    );
    assert_eq!(session.pending_elicitations, vec![request]);

    apply_observation(
        &mut session,
        RelayObservation::ElicitationResolved {
            elicitation_id: "elicitation-1".into(),
            action: "accept".into(),
        },
    );
    assert!(session.pending_elicitations.is_empty());
    assert!(session.transcript.is_empty());
}

#[test]
fn a_plan_decision_also_becomes_a_durable_proposal_item() {
    let mut session = MaterializedSession::empty("session-1");
    let plan = "1. Read the code\n2. Change it";
    let request = mj_core::acp::normalized_plan_review(
        "plan-review-3".into(),
        &serde_json::json!({ "plan": plan }),
    );
    apply_observation(
        &mut session,
        RelayObservation::ElicitationRequested {
            request: request.clone(),
        },
    );

    assert_eq!(session.pending_elicitations, vec![request]);
    assert_eq!(session.transcript.len(), 1);
    let item = &session.transcript[0];
    assert_eq!(item.stable_id, plan_proposal_item_id(1));
    assert_eq!(item.position, 1);
    assert_eq!(
        item.body,
        TranscriptBody::PlanProposal {
            proposal_id: "plan-review-3".into(),
            plan: plan.into(),
        }
    );

    // Answering the decision retires the dialog, not the record of it.
    apply_observation(
        &mut session,
        RelayObservation::ElicitationResolved {
            elicitation_id: "plan-review-3".into(),
            action: "accept".into(),
        },
    );
    assert!(session.pending_elicitations.is_empty());
    assert_eq!(session.transcript.len(), 1);
}

#[test]
fn a_captured_proposal_keeps_its_place_after_the_conversation_that_produced_it() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, untagged_agent_chunk("here is my plan"));
    apply_observation(
        &mut session,
        RelayObservation::ElicitationRequested {
            request: mj_core::acp::normalized_plan_review(
                "plan-review-1".into(),
                &serde_json::json!({ "plan": "do the work" }),
            ),
        },
    );
    apply_observation(&mut session, untagged_agent_chunk("starting now"));

    let bodies = session
        .transcript
        .iter()
        .map(|item| match &item.body {
            TranscriptBody::Agent { .. } => "agent",
            TranscriptBody::PlanProposal { .. } => "proposal",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(bodies, vec!["agent", "proposal", "agent"]);
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

#[test]
fn agent_chunk_while_running_still_streams() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("live"),
                ))
                .message_id("msg-1"),
            )),
        },
    );
    let item = session
        .transcript
        .iter()
        .find(|item| item.stable_id == "agent:msg-1")
        .expect("live chunk recorded");
    assert!(matches!(
        &item.body,
        TranscriptBody::Agent { chunks, streaming }
            if *streaming && crate::transcript::materialized_chunks_text(chunks) == "live"
    ));
}

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
fn idle_untagged_thought_then_agent_chunks_split_into_two_items() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Idle;
    apply_observation(&mut session, untagged_thought_chunk("pondering "));
    apply_observation(&mut session, untagged_thought_chunk("the goal"));
    apply_observation(&mut session, untagged_agent_chunk("here's "));
    apply_observation(&mut session, untagged_agent_chunk("the plan"));

    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(
        &session.transcript[0].body,
        TranscriptBody::Thought { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks) == "pondering the goal"
    ));
    assert!(matches!(
        &session.transcript[1].body,
        TranscriptBody::Agent { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks) == "here's the plan"
    ));
}

#[test]
fn idle_untagged_agent_chunks_split_around_an_intervening_tool_call() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Idle;
    apply_observation(&mut session, untagged_agent_chunk("checking "));
    apply_observation(&mut session, untagged_agent_chunk("the repo"));
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new("call-1", "grep"))),
        },
    );
    apply_observation(&mut session, untagged_agent_chunk("found "));
    apply_observation(&mut session, untagged_agent_chunk("it"));

    let agent_items: Vec<&TranscriptItem> = session
        .transcript
        .iter()
        .filter(|item| matches!(item.body, TranscriptBody::Agent { .. }))
        .map(|item| item.as_ref())
        .collect();
    assert_eq!(agent_items.len(), 2, "transcript: {:?}", session.transcript);
    assert!(matches!(
        &agent_items[0].body,
        TranscriptBody::Agent { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks) == "checking the repo"
    ));
    assert!(matches!(
        &agent_items[1].body,
        TranscriptBody::Agent { chunks, streaming }
            if !*streaming
                && crate::transcript::materialized_chunks_text(chunks) == "found it"
    ));
    assert!(
        session
            .transcript
            .iter()
            .any(|item| matches!(&item.body, TranscriptBody::Tool { .. })),
        "the tool call item survives between the two agent items"
    );
}

#[test]
fn running_untagged_agent_chunks_still_merge_into_one_open_stream() {
    let mut session = MaterializedSession::empty("session-1");
    session.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    for word in ["live ", "streaming ", "text"] {
        apply_observation(&mut session, untagged_agent_chunk(word));
    }
    assert_eq!(session.transcript.len(), 1);
    let item = &session.transcript[0];
    assert!(matches!(
        &item.body,
        TranscriptBody::Agent { chunks, streaming }
            if *streaming
                && crate::transcript::materialized_chunks_text(chunks) == "live streaming text"
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
fn fallback_terminal_tool_completes_in_place_instead_of_parking_output() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, fallback_terminal_tool("term-1", "cargo test"));
    apply_observation(&mut session, terminal_output("term-1"));

    assert_eq!(session.transcript.len(), 1);
    let item = &session.transcript[0];
    assert_eq!(item.stable_id, "tool:hel-terminal:term-1");
    assert_eq!(item.position, 1, "the terminal retains its start order");
    assert_eq!(attached_terminal_outputs(item).len(), 1);
    let TranscriptBody::Tool { call, .. } = &item.body else {
        panic!("the fallback stays a tool");
    };
    let call: ToolCall = serde_json::from_value(call.clone()).unwrap();
    assert_eq!(call.status, ToolCallStatus::Completed);
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
fn terminal_output_after_the_tool_call_attaches_to_the_tool_item() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, terminal_tool_call("call-1", "term-1"));
    apply_observation(&mut session, terminal_output("term-1"));

    assert_eq!(session.transcript.len(), 1, "no standalone item is left");
    let outputs = attached_terminal_outputs(&session.transcript[0]);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].terminal_id, "term-1");
    assert_eq!(outputs[0].output, "build finished\n");
    assert_eq!(outputs[0].exit_code, Some(0));
    assert_eq!(session.transcript[0].last_changed_at_ms, 200);
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
}

#[test]
fn kimi_raw_result_claims_its_unreferenced_terminal_output() {
    const OUTPUT: &str = "toolchain inventory\n";
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCall(ToolCall::new(
                "call-1",
                "Execute `inspect toolchain`",
            ))),
        },
    );
    apply_observation(
        &mut session,
        RelayObservation::TerminalOutput {
            terminal_id: "term-1".into(),
            output: OUTPUT.into(),
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
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(OUTPUT),
                    ))])
                    .raw_output(json!({
                        "type": "Bash",
                        "output": OUTPUT.as_bytes(),
                        "exit_code": 1,
                        "command": "inspect toolchain"
                    })),
            ))),
        },
    );

    assert_eq!(
        session.transcript.len(),
        1,
        "the completed tool consumes the duplicate standalone item"
    );
    let TranscriptBody::Tool {
        terminal_outputs,
        terminal_refs,
        ..
    } = &session.transcript[0].body
    else {
        panic!("the surviving item is the tool call");
    };
    assert_eq!(terminal_refs, &["term-1"]);
    assert_eq!(terminal_outputs.len(), 1);
    assert_eq!(terminal_outputs[0].output, OUTPUT);
    assert_eq!(terminal_outputs[0].exit_code, Some(1));
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
fn queued_prompt_becomes_user_message_only_when_started() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-1".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
            created_at_ms: 100,
        },
    );
    assert!(session.transcript.is_empty());
    assert_eq!(session.queued_prompts.len(), 1);

    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "prompt-1".into(),
            started_at_ms: 200,
        },
    );
    assert!(session.queued_prompts.is_empty());
    assert!(matches!(
        session.transcript[0].body,
        TranscriptBody::User { .. }
    ));

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            command_id: "prompt-1".into(),
            outcome: RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        },
    );
    assert_eq!(session.execution, MaterializedExecutionState::Idle);
}

#[test]
fn first_queued_prompt_seeds_a_provisional_session_title() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
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

    assert_eq!(
        session.session_title.as_deref(),
        Some("fix the flaky resume test")
    );
}

#[test]
fn harness_title_replaces_the_provisional_title() {
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
        RelayObservation::CommandQueued {
            command_id: "prompt-2".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("second prompt"))],
            },
            created_at_ms: 200,
        },
    );
    assert_eq!(session.session_title.as_deref(), Some("first prompt"));

    apply_observation(
        &mut session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::SessionInfoUpdate(
                agent_client_protocol::schema::v1::SessionInfoUpdate::new()
                    .title("Agent-generated title"),
            )),
        },
    );

    assert_eq!(
        session.session_title.as_deref(),
        Some("Agent-generated title")
    );
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
fn next_prompt_backfills_an_existing_untitled_session_from_its_first_prompt() {
    let mut session = MaterializedSession::empty("session-1");
    session.transcript.push(Arc::new(TranscriptItem {
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
    assert_eq!(session.resolved_title().as_deref(), Some("original task"));

    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "prompt-2".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("follow-up task"))],
            },
            created_at_ms: 200,
        },
    );

    assert_eq!(session.session_title.as_deref(), Some("original task"));
}

#[test]
fn queued_config_change_starts_without_becoming_a_turn() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(
        &mut session,
        RelayObservation::CommandQueued {
            command_id: "config-1".into(),
            command: RelayCommand::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            },
            created_at_ms: 100,
        },
    );
    assert_eq!(session.queued_prompts.len(), 1);
    assert_eq!(
        session.queued_prompts[0].kind,
        QueuedCommandKind::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        }
    );
    assert_eq!(
        crate::transcript::materialized_content_text(&session.queued_prompts[0].content),
        "/model sonnet"
    );

    apply_observation(
        &mut session,
        RelayObservation::CommandStarted {
            command_id: "config-1".into(),
            started_at_ms: 200,
        },
    );
    assert!(session.queued_prompts.is_empty());
    assert!(session.transcript.is_empty());
    assert_eq!(session.execution, MaterializedExecutionState::Idle);

    apply_observation(
        &mut session,
        RelayObservation::CommandCompleted {
            command_id: "config-1".into(),
            outcome: RelayCommandOutcome::Configured,
        },
    );
    assert_eq!(session.execution, MaterializedExecutionState::Idle);
    assert!(session.transcript.is_empty());
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
            command_id: "close-1".into(),
            command: RelayCommandKind::Close,
            message: "ACP close failed".into(),
        },
    );
    assert_eq!(session.execution, MaterializedExecutionState::Idle);
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

#[test]
fn one_chunk_projects_only_the_touched_logical_item() {
    let mut session = MaterializedSession::empty("session-1");
    session.applied_event_ordinal = 10_000;
    session.applied_event_digest = "a".repeat(64);
    session.last_activity_at_ms = Some(10_000);
    session.transcript = (1..=10_000)
        .map(|position| {
            Arc::new(TranscriptItem {
                stable_id: format!("system:{position}"),
                position,
                latest_content_event_ordinal: None,
                created_at_ms: i64::try_from(position).unwrap(),
                last_changed_at_ms: i64::try_from(position).unwrap(),
                body: TranscriptBody::System {
                    text: format!("event {position}"),
                },
            })
        })
        .collect();
    let next = event(
        &session,
        RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::AgentMessageChunk(
                agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(
                    TextContent::new("answer"),
                ))
                .message_id("answer-1"),
            )),
        },
    );

    let projected = project_relay_event(&session, &next).unwrap();

    assert_eq!(projected.mutation.transcript.len(), 1);
    assert!(projected.mutation.configuration.is_none());
    assert!(projected.mutation.queued_prompts.is_none());
    assert_eq!(session.transcript.len(), 10_000);
    apply_committed_projection_event(&mut session, &next, projected.mutation).unwrap();
    assert_eq!(session.transcript.len(), 10_001);
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

fn end_turn() -> RelayObservation {
    RelayObservation::CommandCompleted {
        command_id: "prompt-1".into(),
        outcome: RelayCommandOutcome::Prompt {
            diagnostic: None,
            stop_reason: "end_turn".into(),
            usage: None,
        },
    }
}

fn agent_text(item: &TranscriptItem) -> String {
    let TranscriptBody::Agent { chunks, .. } = &item.body else {
        panic!("expected an agent message, got {:?}", item.body);
    };
    crate::transcript::materialized_chunks_text(chunks)
}

#[test]
fn appending_a_transcript_item_leaves_earlier_items_shared_with_older_snapshots() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, agent_chunk("answer", "answer-1"));
    apply_observation(&mut session, end_turn());
    let published = session.clone();

    apply_observation(
        &mut session,
        RelayObservation::Warning {
            message: "disk is nearly full".into(),
        },
    );

    assert_eq!(published.transcript.len(), 1);
    assert_eq!(session.transcript.len(), 2);
    assert!(matches!(
        &session.transcript[1].body,
        TranscriptBody::System { text } if text == "warning: disk is nearly full"
    ));
    assert!(
        Arc::ptr_eq(&session.transcript[0], &published.transcript[0]),
        "cloning a session must share earlier transcript items, not copy them"
    );
}

#[test]
fn appending_a_chunk_replaces_only_the_streaming_tail_item() {
    let mut session = MaterializedSession::empty("session-1");
    apply_observation(&mut session, agent_chunk("finished", "answer-1"));
    apply_observation(&mut session, end_turn());
    apply_observation(&mut session, agent_chunk("hel", "answer-2"));
    let published = session.clone();

    apply_observation(&mut session, agent_chunk("lo", "answer-2"));

    assert_eq!(session.transcript.len(), 2);
    assert!(
        Arc::ptr_eq(&session.transcript[0], &published.transcript[0]),
        "finalized items stay shared while the tail streams"
    );
    assert!(
        !Arc::ptr_eq(&session.transcript[1], &published.transcript[1]),
        "the streaming tail must be replaced, not mutated in place"
    );
    assert_eq!(agent_text(&published.transcript[1]), "hel");
    assert_eq!(agent_text(&session.transcript[1]), "hello");
}

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

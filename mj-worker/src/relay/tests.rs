use std::sync::{Arc, Mutex};

use super::background::{CLAUDE_STOP_ACKNOWLEDGEMENT_PREFIX, agent_chunk_text};
use super::*;

#[test]
fn idle_upgrade_reservation_defers_without_delaying_steering() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
    submit_relay(&mut relay, "running-command", prompt("work"));
    relay.claim_pending_commands(true).unwrap();
    submit_relay(&mut relay, "next-command", prompt("change direction"));
    let before = relay.operational_state().latest_ordinal;
    let response = relay.handle(relay_request(
        "reserve",
        RelayRequest::ReserveIdle {
            command_id: "upgrade-command".into(),
        },
    ));
    assert!(matches!(
        response.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::IdleReservation { ordinal: None }
        }
    ));
    assert_eq!(relay.operational_state().latest_ordinal, before);
    assert!(relay.operational_state().checkpoint_barrier.is_none());
    submit_relay(
        &mut relay,
        "steer-command",
        RelayCommand::Steer {
            active_prompt_id: "running-command".into(),
            queued_prompt_id: "next-command".into(),
        },
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "steer-command");
    assert_eq!(
        claimed[0]
            .steering_prompt
            .as_ref()
            .unwrap()
            .queued_command_id,
        "next-command"
    );
}

#[test]
fn idle_upgrade_reservation_is_idempotent_and_disconnect_releases_queued_work() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    let reserve = RelayRequest::ReserveIdle {
        command_id: "upgrade-command".into(),
    };
    let response = relay.handle(relay_request("reserve", reserve.clone()));
    let RelayResponseBody::Ok {
        payload: RelayResponsePayload::IdleReservation {
            ordinal: Some(first),
        },
    } = response.body
    else {
        panic!("idle worker refused reservation: {:?}", response.body);
    };
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    relay.record_checkpoint_ready("upgrade-command").unwrap();
    let response = relay.handle(relay_request("retry", reserve));
    assert!(matches!(response.body, RelayResponseBody::Ok {
        payload: RelayResponsePayload::IdleReservation { ordinal: Some(ordinal) }
    } if ordinal == first));
    submit_relay(&mut relay, "after-command", prompt("next turn"));
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    relay
        .cancel_checkpoint_barrier_on_disconnect("upgrade-command")
        .unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "after-command");
}

#[test]
fn classifier_input_handoff_preserves_running_children_and_stop_controls() {
    use mj_core::native_agent::{NativeAgentCapabilities, NativeAgentEvent, NativeAgentState};
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
    relay.set_background_work_policy(BackgroundWorkPolicy::ClaudeTasks);
    submit_relay(
        &mut relay,
        "approval",
        prompt("Prepare deployment while analyzing the heap independently."),
    );
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_observation(RelayObservation::NativeAgent {
            event: NativeAgentEvent::Spawned {
                session_id: "heap".into(),
                parent_session_id: None,
                name: "heap".into(),
                task: "Independent heap analysis".into(),
                capabilities: NativeAgentCapabilities {
                    cancel: true,
                    close: false,
                },
            },
        })
        .unwrap();
    relay
        .record_observation(RelayObservation::NativeAgent {
            event: NativeAgentEvent::State {
                session_id: "heap".into(),
                state: NativeAgentState::Running,
            },
        })
        .unwrap();
    relay
        .claude_background_tasks_changed(vec![claude_task("build", "Independent build")])
        .unwrap();
    relay
        .claude_async_task_control_changed("build".into(), true)
        .unwrap();
    let before = relay.operational_state();
    assert_eq!(before.native_agent_count, 1);
    relay
        .record_command_completed(
            "approval",
            RelayCommandOutcome::Prompt {
                stop_reason: mj_core::acp::AWAITING_INPUT_STOP_REASON.into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    let after = relay.operational_state();
    assert!(after.activity_state().is_idle(), "{after:?}");
    assert_eq!(after.native_agents, before.native_agents);
    assert_eq!(after.native_agent_count, 1);
    assert_eq!(after.background_commands, before.background_commands);
    assert!(
        relay
            .background_task_stop_target("native-agent:heap")
            .is_ok()
    );
    assert!(relay.background_task_stop_target("claude:build").is_ok());
    assert!(!after.is_quiet());
    assert!(relay.pending_replied_verdict().is_none());
    // Repeated child output must not undo the accepted parent handoff.
    relay.record_observation(RelayObservation::NativeAgent { event: NativeAgentEvent::Update {
        session_id: "heap".into(), update: Box::new(serde_json::from_value(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"Still analyzing."}
        })).unwrap()),
    }}).unwrap();
    assert!(relay.operational_state().activity_state().is_idle());
}

#[test]
fn native_replay_does_not_publish_provisional_work_or_lose_retained_agents() {
    use mj_core::native_agent::{NativeAgentEvent, NativeAgentState};
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    let spawn = |id: &str| NativeAgentEvent::Spawned {
        session_id: id.into(),
        parent_session_id: None,
        name: id.into(),
        task: "task".into(),
        capabilities: Default::default(),
    };
    for event in [
        spawn("retained"),
        NativeAgentEvent::State {
            session_id: "retained".into(),
            state: NativeAgentState::Completed,
        },
        NativeAgentEvent::ReplayBegin,
        spawn("replayed"),
    ] {
        relay
            .record_observation(RelayObservation::NativeAgent { event })
            .unwrap();
    }
    assert_eq!(relay.operational_state().native_agent_count, 0);
    assert_eq!(relay.operational_state().native_agents.len(), 1);
    relay
        .record_observation(RelayObservation::NativeAgent {
            event: NativeAgentEvent::ReplayCommit,
        })
        .unwrap();
    assert_eq!(relay.operational_state().native_agents.len(), 2);
    assert_eq!(relay.operational_state().native_agent_count, 0);
    relay
        .record_observation(RelayObservation::NativeAgent {
            event: NativeAgentEvent::Disconnected,
        })
        .unwrap();
    assert_eq!(relay.operational_state().native_agent_count, 0);
}

#[test]
fn uncertain_steering_survives_restart_and_requires_explicit_retry() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    submit_relay(
        &mut relay,
        "prompt-first",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("first")],
        },
    );
    relay.claim_pending_commands(true).unwrap();
    submit_relay(
        &mut relay,
        "prompt-queued",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("next")],
        },
    );
    submit_relay(
        &mut relay,
        "checkpoint-earlier",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    let steer = RelayCommand::Steer {
        active_prompt_id: "prompt-first".into(),
        queued_prompt_id: "prompt-queued".into(),
    };
    submit_relay(&mut relay, "steer-first", steer.clone());
    assert!(
        relay
            .submit_command("steer-repeat", steer)
            .unwrap()
            .is_err()
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert!(relay.snapshot.checkpoint_barrier.is_none());
    assert_eq!(
        claimed[0]
            .steering_prompt
            .as_ref()
            .unwrap()
            .queued_command_id,
        "prompt-queued"
    );
    relay
        .record_command_interrupted("steer-first", "connection lost")
        .unwrap();
    relay
        .record_command_interrupted("prompt-first", "harness restarted")
        .unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    assert!(
        relay
            .submit_command(
                "checkpoint-held",
                RelayCommand::BeginCheckpoint { reason: None }
            )
            .unwrap()
            .is_err()
    );
    drop(relay);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    assert_eq!(
        relay.operational_state().steering.unwrap().status,
        mj_core::relay::SteeringStatus::Unconfirmed
    );
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    submit_relay(
        &mut relay,
        "resolve-steering",
        RelayCommand::ResolveSteering {
            steering_id: "steer-first".into(),
        },
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "prompt-queued");
}

/// A relay whose bridge returns steers it cannot inject, with `prompt-first`
/// running.
fn relay_steering_automatically(root: &std::path::Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "test").unwrap();
    relay.set_steering_supported(Some(true));
    relay.set_automatic_steering(true);
    submit_relay(
        &mut relay,
        "prompt-first",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("first")],
        },
    );
    relay.claim_pending_commands(true).unwrap();
    relay
}

fn queue_prompt(relay: &mut DurableRelay, command_id: &str, text: &str) {
    submit_relay(
        relay,
        command_id,
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from(text)],
        },
    );
}

fn claimed_steer_of(claimed: &[ClaimedRelayCommand], queued: &str) -> String {
    let [steer] = claimed else {
        panic!("expected one automatic steer, got {claimed:?}");
    };
    assert!(steer.command_id.starts_with("auto-steer-"));
    assert!(matches!(
        &steer.command,
        RelayCommand::Steer { active_prompt_id, queued_prompt_id }
            if active_prompt_id == "prompt-first" && queued_prompt_id == queued
    ));
    assert_eq!(
        steer.steering_prompt.as_ref().unwrap().queued_command_id,
        queued
    );
    steer.command_id.clone()
}

#[test]
fn queued_prompts_are_steered_into_the_running_turn_one_at_a_time() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = relay_steering_automatically(temp.path());
    queue_prompt(&mut relay, "prompt-second", "also this");
    let steer = claimed_steer_of(
        &relay.claim_pending_commands(true).unwrap(),
        "prompt-second",
    );
    assert_eq!(
        relay.operational_state().steering.unwrap().status,
        mj_core::relay::SteeringStatus::Pending
    );

    queue_prompt(&mut relay, "prompt-third", "and this");
    assert!(
        relay.claim_pending_commands(true).unwrap().is_empty(),
        "a second steer waits for the first to settle"
    );

    relay
        .record_command_completed(
            &steer,
            RelayCommandOutcome::Steered {
                queued_command_id: "prompt-second".into(),
            },
        )
        .unwrap();
    claimed_steer_of(&relay.claim_pending_commands(true).unwrap(), "prompt-third");
}

#[test]
fn automatic_steering_stops_for_a_turn_that_returned_a_steer() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = relay_steering_automatically(temp.path());
    queue_prompt(&mut relay, "prompt-second", "also this");
    let steer = claimed_steer_of(
        &relay.claim_pending_commands(true).unwrap(),
        "prompt-second",
    );
    relay
        .record_command_completed(
            &steer,
            RelayCommandOutcome::SteeringReturned {
                queued_command_id: "prompt-second".into(),
            },
        )
        .unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    // The returned prompt runs as the next turn.
    relay
        .record_command_completed(
            "prompt-first",
            RelayCommandOutcome::Prompt {
                stop_reason: "EndTurn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "prompt-second");
}

#[test]
fn slash_commands_and_waiting_checkpoints_are_not_steered() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = relay_steering_automatically(temp.path());
    queue_prompt(&mut relay, "prompt-review", "/review the parser");
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    let temp = tempfile::tempdir().unwrap();
    let mut relay = relay_steering_automatically(temp.path());
    submit_relay(
        &mut relay,
        "checkpoint-1",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    queue_prompt(&mut relay, "prompt-second", "/home/me/notes.txt is wrong");
    assert!(
        !relay
            .claim_pending_commands(true)
            .unwrap()
            .iter()
            .any(|claimed| matches!(claimed.command, RelayCommand::Steer { .. })),
        "a checkpoint is waiting for this turn to end"
    );
}

#[test]
fn bridges_that_start_their_own_turns_are_not_steered_automatically() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = relay_steering_automatically(temp.path());
    relay.set_automatic_steering(false);
    queue_prompt(&mut relay, "prompt-second", "also this");
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    assert!(relay.operational_state().steering.is_none());
}

#[test]
fn a_path_is_a_message_and_a_command_name_is_not() {
    let prompt = |text: &str| vec![ContentBlock::from(text)];
    assert!(mj_core::acp::prompt_is_slash_command(&prompt("/review")));
    assert!(mj_core::acp::prompt_is_slash_command(&prompt(
        "  /plugin:run now"
    )));
    assert!(!mj_core::acp::prompt_is_slash_command(&prompt(
        "/home/me/file"
    )));
    assert!(!mj_core::acp::prompt_is_slash_command(&prompt("/ nothing")));
    assert!(!mj_core::acp::prompt_is_slash_command(&prompt(
        "fix /review"
    )));
}

#[test]
fn a_returned_steer_keeps_the_prompt_queued_for_the_next_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    submit_relay(
        &mut relay,
        "prompt-first",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("first")],
        },
    );
    relay.claim_pending_commands(true).unwrap();
    submit_relay(
        &mut relay,
        "prompt-queued",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("next")],
        },
    );
    submit_relay(
        &mut relay,
        "steer-first",
        RelayCommand::Steer {
            active_prompt_id: "prompt-first".into(),
            queued_prompt_id: "prompt-queued".into(),
        },
    );
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(
            "steer-first",
            RelayCommandOutcome::SteeringReturned {
                queued_command_id: "prompt-queued".into(),
            },
        )
        .unwrap();

    // Nothing was delivered and nothing waits on the user.
    let state = relay.operational_state();
    let steering = state.steering.unwrap();
    assert_eq!(steering.status, mj_core::relay::SteeringStatus::Resolved);
    assert_eq!(steering.message, None);
    assert_eq!(state.queued_prompts.len(), 1);
    assert_eq!(state.queued_prompts[0].command_id, "prompt-queued");

    relay
        .record_command_completed(
            "prompt-first",
            RelayCommandOutcome::Prompt {
                stop_reason: "EndTurn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "prompt-queued");
}

#[test]
fn steering_rejects_changed_queue_and_consumes_late_confirmed_input_once() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    submit_relay(
        &mut relay,
        "prompt-first",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("first")],
        },
    );
    relay.claim_pending_commands(true).unwrap();
    submit_relay(
        &mut relay,
        "prompt-queued",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("next")],
        },
    );
    assert!(
        relay
            .submit_command(
                "steer-stale",
                RelayCommand::Steer {
                    active_prompt_id: "prompt-first".into(),
                    queued_prompt_id: "changed-queue".into()
                }
            )
            .unwrap()
            .is_err()
    );
    submit_relay(
        &mut relay,
        "steer-first",
        RelayCommand::Steer {
            active_prompt_id: "prompt-first".into(),
            queued_prompt_id: "prompt-queued".into(),
        },
    );
    relay.claim_pending_commands(true).unwrap();
    assert!(
        relay
            .submit_command(
                "remove-pending",
                RelayCommand::RemoveQueuedPrompt {
                    queued_command_id: "prompt-queued".into()
                }
            )
            .unwrap()
            .is_err()
    );
    relay
        .record_observation(RelayObservation::SteeringUnconfirmed {
            command_id: "steer-first".into(),
            message: "slow acknowledgment".into(),
        })
        .unwrap();
    relay
        .record_command_completed(
            "steer-first",
            RelayCommandOutcome::Steered {
                queued_command_id: "prompt-queued".into(),
            },
        )
        .unwrap();
    assert!(relay.operational_state().queued_prompts.is_empty());
    assert_eq!(
        relay.operational_state().steering.unwrap().status,
        mj_core::relay::SteeringStatus::Applied
    );
    relay
        .record_command_interrupted("prompt-first", "stopped")
        .unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    assert!(
        relay
            .submit_command(
                "cancel-stale",
                RelayCommand::CancelTurnFor {
                    active_prompt_id: "prompt-first".into()
                }
            )
            .unwrap()
            .is_err()
    );
}
use test_support::*;

#[test]
fn a_locally_created_empty_native_session_has_no_history() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "unused".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    assert!(!relay.native_session_may_have_history());
    // A prompt waiting in the durable queue never reached the agent.
    submit_relay(
        &mut relay,
        "queued-prompt",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("queued work")],
        },
    );
    assert!(!relay.native_session_may_have_history());
}

#[test]
fn a_dispatched_prompt_gives_the_native_session_history() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "used".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    submit_relay(
        &mut relay,
        "dispatched-prompt",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("do work")],
        },
    );
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    assert!(relay.native_session_may_have_history());
}

#[test]
fn a_released_recovery_floor_gives_the_native_session_history() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "unused".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    assert!(!relay.native_session_may_have_history());
    let cursor = ready_checkpoint(&mut relay, "checkpoint");
    submit_floor(&mut relay, "archive-installed", cursor);
    assert!(relay.native_session_may_have_history());
}

#[test]
fn a_native_session_opened_after_a_restore_floor_has_no_history() {
    // A moved session starts from an archive seed, which sets the recovery
    // floor to the archive's frontier, and then opens a brand-new native
    // session above it. Nothing in the archive can belong to that session.
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        mj_core::relay::restored_relay_seed_path(temp.path()),
        serde_json::to_vec(&serde_json::json!({
            "event_frontier": 227_580,
            "event_frontier_digest":
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        }))
        .unwrap(),
    )
    .unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.snapshot.recovery_floor_ordinal, 227_580);
    // Before any session is opened the answer is unknown, so conservative.
    assert!(relay.native_session_may_have_history());
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "fresh".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    assert!(!relay.native_session_may_have_history());

    // The opening ordinal must survive a worker restart and journal replay.
    drop(relay);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(!relay.native_session_may_have_history());

    submit_relay(
        &mut relay,
        "dispatched-prompt",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("do work")],
        },
    );
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    assert!(relay.native_session_may_have_history());
}

#[test]
fn a_resumed_native_session_has_history_after_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "imported".into(),
            resumed: true,
            native_continuity_lost: false,
        })
        .unwrap();
    assert!(relay.native_session_may_have_history());
    drop(relay);
    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(relay.native_session_may_have_history());
}

#[test]
fn a_used_native_session_stays_used_across_persist_and_replay() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "used".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    relay.mark_native_session_used().unwrap();
    // Transcript events after the mark are replayed on reopen, and replay
    // must not undo it.
    relay
        .record_observation(RelayObservation::Warning {
            message: "after the mark".into(),
        })
        .unwrap();
    drop(relay);
    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(relay.native_session_may_have_history());
    assert!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(temp.path().join("relay-state.json")).unwrap()
        )
        .unwrap()["native_session_used"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn lost_native_continuity_survives_reopen_and_clears_on_a_normal_open() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(!relay.operational_state().native_continuity_lost);
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "fresh".into(),
            resumed: false,
            native_continuity_lost: true,
        })
        .unwrap();
    assert!(relay.operational_state().native_continuity_lost);
    drop(relay);

    // A controller that reconnects later still sees it.
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(relay.operational_state().native_continuity_lost);

    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "later".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    assert!(!relay.operational_state().native_continuity_lost);
}

#[test]
fn journals_without_lost_native_continuity_default_to_intact() {
    let observation = serde_json::json!({
        "type": "session_opened",
        "data": {"native_session_id": "native", "resumed": false},
    });
    let observation: RelayObservation = serde_json::from_value(observation).unwrap();
    assert_eq!(
        observation,
        RelayObservation::SessionOpened {
            native_session_id: "native".into(),
            resumed: false,
            native_continuity_lost: false,
        }
    );

    let mut snapshot = RelaySnapshot::new(SESSION.to_owned());
    snapshot.native_continuity_lost = true;
    let mut stored = serde_json::to_value(&snapshot).unwrap();
    stored
        .as_object_mut()
        .unwrap()
        .remove("native_continuity_lost")
        .expect("the flag is serialized");
    let legacy: RelaySnapshot = serde_json::from_value(stored).unwrap();
    assert!(!legacy.native_continuity_lost);
    assert!(!legacy.operational_state().native_continuity_lost);
}

#[test]
fn hidden_prompt_context_is_removed_from_harness_visible_text() {
    let text = concat!(
        "<mj-project-memory>private memory</mj-project-memory>\n\n",
        "<user_shell_command>private output</user_shell_command>\n",
        "ship the visible change"
    );

    assert_eq!(strip_hidden_prompt_context(text), "ship the visible change");
    assert_eq!(
        strip_hidden_prompt_context("<mj-project-memory>truncated"),
        ""
    );
    assert_eq!(
        strip_hidden_prompt_context("<user-request>keep me</user-request>"),
        "<user-request>keep me</user-request>"
    );
}

#[test]
fn acp_activity_clock_is_shared_with_operational_status_but_not_persisted() {
    let temp = tempfile::tempdir().unwrap();
    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.operational_state().last_acp_activity_at_ms, None);
    relay.acp_activity_clock().mark();
    assert!(relay.operational_state().last_acp_activity_at_ms.is_some());

    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(reopened.operational_state().last_acp_activity_at_ms, None);
}

/// The worker assembles activity facts directly, because the idle clock runs
/// on every journal append and building a whole operational state there would
/// clone the session configuration once per streamed chunk. That makes two
/// places that translate a session into facts, so this pins them together: if
/// one ever stops reporting a fact the other reports, this fails.
#[test]
fn worker_facts_match_the_published_state() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        relay.activity_facts(),
        relay.operational_state().facts(),
        "a fresh relay"
    );

    relay.acp_activity_clock().mark();
    // The handle the ACP driver's watchdog holds is taken before the session
    // runs, exactly as the worker runtime takes it, and must see what the
    // relay records afterwards. If these ever stop being the same tracker,
    // the watchdog goes blind and a turn blocked in a long tool call is
    // failed again (#1020).
    let watchdog_view = relay.tools_in_flight();
    assert!(watchdog_view.is_empty());
    relay
        .record_observation(RelayObservation::HarnessTurnStarted {
            started_at_ms: 1_234,
        })
        .unwrap();
    relay.record_session_update(tool_call_update()).unwrap();
    let published = relay.operational_state();
    assert_eq!(relay.activity_facts(), published.facts(), "a working relay");
    assert_eq!(
        published.activity.as_ref(),
        Some(&mj_core::activity::classify(&published.facts())),
        "the published answer is the one a consumer would compute"
    );
    assert_eq!(published.tools_in_flight.len(), 1);
    assert_eq!(
        watchdog_view.snapshot(),
        published.tools_in_flight,
        "the watchdog's handle and the published list are one tracker"
    );
    assert!(!published.activity_state().is_idle());
}

#[test]
fn restored_native_identity_waits_for_current_acp_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "native-session".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    assert!(!relay.operational_state().native_session_is_ready());
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    assert!(relay.operational_state().native_session_is_ready());
    drop(relay);

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        relay.operational_state().native_session_id.as_deref(),
        Some("native-session")
    );
    assert_eq!(relay.operational_state().acp_ready, Some(false));
    assert!(!relay.operational_state().native_session_is_ready());
    for transition in [
        RelayObservation::SessionRestarted,
        RelayObservation::Closing,
        RelayObservation::Closed,
    ] {
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        assert_eq!(relay.operational_state().acp_ready, Some(true));
        relay.record_observation(transition).unwrap();
        assert_eq!(relay.operational_state().acp_ready, Some(false));
    }
}

#[test]
fn legacy_snapshot_keeps_its_live_turn_when_activity_clocks_are_upgraded() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    relay
        .record_observation(RelayObservation::HarnessTurnStarted {
            started_at_ms: 12_000,
        })
        .unwrap();
    let mut legacy = serde_json::to_value(&relay.snapshot).unwrap();
    legacy["format_version"] = serde_json::json!(4);
    legacy
        .as_object_mut()
        .unwrap()
        .remove("activity_turn_started_at_ms");
    drop(relay);
    fs::write(
        temp.path().join(RELAY_STATE_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    assert_eq!(relay.snapshot.format_version, RELAY_STATE_VERSION);
    assert_eq!(
        relay.operational_state().activity_turn_started_at_ms,
        Some(12_000)
    );
    assert_eq!(
        relay
            .operational_state()
            .harness_turn
            .unwrap()
            .started_at_ms,
        12_000
    );
    relay.persist_snapshot().unwrap();
    drop(relay);
    let reopened = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    assert_eq!(
        reopened.operational_state().activity_turn_started_at_ms,
        Some(12_000)
    );
}

#[test]
fn idle_clock_starts_at_settlement_survives_reopen_and_ignores_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    assert_eq!(relay.operational_state().idle_since_ms, None);
    relay.record_session_update(tool_call_update()).unwrap();
    assert_eq!(relay.operational_state().idle_since_ms, None);
    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    let idle_since = relay.operational_state().idle_since_ms;
    assert!(idle_since.is_some());
    relay
        .record_observation(RelayObservation::Warning {
            message: "metadata does not change activity".into(),
        })
        .unwrap();
    assert_eq!(relay.operational_state().idle_since_ms, idle_since);
    drop(relay);
    let mut relay = claude_relay(temp.path());
    assert_eq!(relay.operational_state().idle_since_ms, idle_since);
    relay.record_session_update(tool_call_update()).unwrap();
    assert_eq!(relay.operational_state().idle_since_ms, None);
}

#[test]
fn relay_store_identity_survives_restart_but_distinguishes_a_fresh_destination() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let relay = DurableRelay::open(&first, "session-one", "test").unwrap();
    let identity = relay.operational_state().store_id.unwrap();
    drop(relay);
    let reopened = DurableRelay::open(&first, "session-one", "test").unwrap();
    assert_eq!(
        reopened.operational_state().store_id.as_deref(),
        Some(identity.as_str())
    );
    let fresh = DurableRelay::open(directory.path().join("fresh"), "session-one", "test").unwrap();
    assert_ne!(
        fresh.operational_state().store_id.as_deref(),
        Some(identity.as_str())
    );
}

#[test]
fn idle_clock_waits_for_background_work_and_persists_its_completion() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();
    let turn_start = relay.operational_state().activity_turn_started_at_ms;
    assert!(turn_start.is_some());
    relay
        .agent_terminal_started(ActiveAgentTerminal {
            terminal_id: "background".into(),
            command: "build".into(),
            started_at_ms: 1_000,
        })
        .unwrap();
    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    assert_eq!(relay.operational_state().idle_since_ms, None);
    assert_eq!(
        relay.operational_state().activity_turn_started_at_ms,
        turn_start
    );
    // A newly attached client receives the original clock from the worker.
    let reattached: RelayOperationalState =
        serde_json::from_slice(&serde_json::to_vec(&relay.operational_state()).unwrap()).unwrap();
    assert_eq!(reattached.activity_turn_started_at_ms, turn_start);
    let stored: RelaySnapshot =
        serde_json::from_slice(&fs::read(temp.path().join(RELAY_STATE_FILE)).unwrap()).unwrap();
    assert_eq!(stored.activity_turn_started_at_ms, turn_start);
    let next_turn = turn_start.unwrap() + 1_000;
    relay
        .record_observation(RelayObservation::HarnessTurnStarted {
            started_at_ms: next_turn,
        })
        .unwrap();
    assert_eq!(
        relay.operational_state().activity_turn_started_at_ms,
        Some(next_turn)
    );
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    assert_eq!(
        relay.operational_state().activity_turn_started_at_ms,
        Some(next_turn)
    );
    relay.agent_terminal_closed("background").unwrap();
    assert_eq!(relay.operational_state().activity_turn_started_at_ms, None);
    let idle_since = relay.operational_state().idle_since_ms;
    assert!(idle_since.is_some());
    drop(relay);
    assert_eq!(
        claude_relay(temp.path()).operational_state().idle_since_ms,
        idle_since
    );
}

#[test]
fn legacy_idle_snapshot_does_not_invent_an_idle_start() {
    let temp = tempfile::tempdir().unwrap();
    let relay = claude_relay(temp.path());
    let mut value = serde_json::to_value(&relay.snapshot).unwrap();
    value.as_object_mut().unwrap().remove("idle_since_ms");
    value.as_object_mut().unwrap().remove("activity_was_idle");
    drop(relay);
    fs::write(
        temp.path().join(RELAY_STATE_FILE),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let mut relay = claude_relay(temp.path());
    relay
        .record_observation(RelayObservation::Warning {
            message: "old worker".into(),
        })
        .unwrap();
    assert_eq!(relay.operational_state().idle_since_ms, None);
    let mut operational = serde_json::to_value(relay.operational_state()).unwrap();
    operational.as_object_mut().unwrap().remove("idle_since_ms");
    assert_eq!(
        serde_json::from_value::<RelayOperationalState>(operational)
            .unwrap()
            .idle_since_ms,
        None
    );
}

#[test]
fn step_clock_is_shared_with_operational_status_but_not_persisted() {
    let temp = tempfile::tempdir().unwrap();
    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.operational_state().current_step_started_at_ms, None);
    relay.step_clock().begin_turn();
    assert!(
        relay
            .operational_state()
            .current_step_started_at_ms
            .is_some()
    );
    relay.step_clock().end_turn();
    assert_eq!(
        relay.operational_state().current_step_started_at_ms,
        None,
        "a finished turn leaves no step in flight"
    );

    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        reopened.operational_state().current_step_started_at_ms,
        None
    );
}

#[test]
fn hidden_context_waits_for_a_prompt_and_survives_an_interruption() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let installed = relay.handle(relay_request(
        "install-context",
        RelayRequest::InstallPromptContext {
            text: "<hel-background>memory</hel-background>".into(),
        },
    ));
    assert!(matches!(
        installed.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::PromptContextInstalled
        }
    ));

    submit_relay(
        &mut relay,
        "configure-first",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "default".into(),
        },
    );
    let config = relay.claim_pending_commands(true).unwrap();
    assert_eq!(config.len(), 1);
    assert_eq!(config[0].hidden_prompt_context, None);
    relay
        .record_command_completed("configure-first", RelayCommandOutcome::Configured)
        .unwrap();

    submit_relay(&mut relay, "first-prompt", prompt("do it"));
    let first = relay.claim_pending_commands(true).unwrap();
    assert_eq!(
        first[0].hidden_prompt_context.as_deref(),
        Some("<hel-background>memory</hel-background>")
    );
    assert_eq!(first[0].command, prompt("do it"));
    relay
        .record_command_interrupted("first-prompt", "restart")
        .unwrap();

    submit_relay(&mut relay, "second-prompt", prompt("continue"));
    let second = relay.claim_pending_commands(true).unwrap();
    assert_eq!(
        second[0].hidden_prompt_context.as_deref(),
        Some("<hel-background>memory</hel-background>")
    );
    relay
        .record_command_completed(
            "second-prompt",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();

    submit_relay(&mut relay, "third-prompt", prompt("again"));
    let third = relay.claim_pending_commands(true).unwrap();
    assert_eq!(third[0].hidden_prompt_context, None);
}

/// A history large enough to seal several segments and to overflow one
/// replay page, so an attach against it really does read and decompress.
fn record_paged_history(relay: &mut DurableRelay, events: usize) {
    for index in 0..events {
        relay
            .record_observation(RelayObservation::Warning {
                message: format!("{index:04}:{}", "x".repeat(64 * 1024)),
            })
            .unwrap();
    }
}

fn attach_envelope(
    relay: &DurableRelay,
    request_id: &str,
    after_ordinal: u64,
) -> RelayRequestEnvelope {
    relay_request(
        request_id,
        RelayRequest::Attach {
            after_ordinal,
            after_digest: relay.digest_at(after_ordinal).unwrap().unwrap(),
        },
    )
}

#[test]
fn a_deferred_attach_reads_its_page_while_the_relay_keeps_recording() {
    let temp = tempfile::tempdir().unwrap();
    let relay = Arc::new(Mutex::new(
        DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap(),
    ));
    record_paged_history(&mut relay.lock().unwrap(), 80);
    let planned_frontier = relay.lock().unwrap().latest_ordinal();

    let deferred = {
        let guard = relay.lock().unwrap();
        guard
            .take_deferred_attach(&attach_envelope(&guard, "catch-up", 0))
            .expect("an attach is deferred off the relay lock")
    };

    // The catch-up page has not been assembled yet, and the relay is free.
    for index in 0..8 {
        relay
            .lock()
            .expect("the relay lock is not held by the pending replay")
            .record_observation(RelayObservation::Warning {
                message: format!("live-{index}"),
            })
            .unwrap();
    }
    let live_frontier = relay.lock().unwrap().latest_ordinal();
    assert_eq!(live_frontier, planned_frontier + 8);

    let response = deferred.finish();
    let RelayResponseBody::Ok {
        payload:
            RelayResponsePayload::Attached {
                events,
                through_ordinal,
                through_digest,
                state,
            },
    } = response.body
    else {
        panic!("deferred attach failed");
    };
    assert!(
        !events.is_empty() && through_ordinal < planned_frontier,
        "the history should not fit in one page: {through_ordinal} of {planned_frontier}"
    );
    // The page is one unbroken run of the chain the cursor asked for, and
    // it reports the frontier captured with the plan rather than the one
    // the live appends moved it to.
    let mut cursor = RelayCursor {
        ordinal: 0,
        digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
    };
    for event in &events {
        validate_relay_event(cursor.ordinal, &cursor.digest, event).unwrap();
        cursor.ordinal = event.ordinal;
        cursor.digest.clone_from(&event.digest);
    }
    assert_eq!(cursor.ordinal, through_ordinal);
    assert_eq!(cursor.digest, through_digest);
    assert_eq!(state.latest_ordinal, planned_frontier);
}

#[test]
fn a_proven_replay_cursor_does_not_reread_its_old_segment() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    record_paged_history(&mut relay, 80);
    let first_request = attach_envelope(&relay, "first", 0);
    let first = relay.handle(first_request);
    let RelayResponseBody::Ok {
        payload:
            RelayResponsePayload::Attached {
                through_ordinal,
                through_digest,
                ..
            },
    } = &first.body
    else {
        panic!("first replay page failed: {:?}", first.body);
    };
    assert!(*through_ordinal < relay.latest_ordinal());
    relay.remember_replay_cursor(&first);

    let old_segment = relay
        .journal_spans
        .iter()
        .find(|span| {
            *through_ordinal > span.after_ordinal && *through_ordinal <= span.file_last_ordinal
        })
        .expect("the replay cursor belongs to a journal segment")
        .path
        .clone();
    std::fs::rename(&old_segment, old_segment.with_extension("moved")).unwrap();

    assert_eq!(
        relay.digest_at(*through_ordinal).unwrap().as_deref(),
        Some(through_digest.as_str()),
        "validating the returned cursor must not reopen its old segment"
    );
}

#[test]
fn a_deferred_attach_refuses_a_page_from_a_collected_journal() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    record_paged_history(&mut relay, 80);
    let sealed = |root: &Path| {
        fs::read_dir(root.join(RELAY_JOURNAL_DIR))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|name| name == "gz"))
            .count()
    };
    assert!(sealed(temp.path()) >= 2, "history did not seal segments");

    let deferred = relay
        .take_deferred_attach(&attach_envelope(&relay, "catch-up", 0))
        .expect("an attach is deferred off the relay lock");
    let generation = deferred.journal_generation();

    // Another controller acknowledges the whole history, which rewrites
    // the journal and deletes every sealed segment this plan named.
    let frontier = relay.latest_ordinal();
    let frontier_digest = relay.latest_digest().to_owned();
    submit_floor(
        &mut relay,
        "floor-command-0001",
        RelayCursor {
            ordinal: frontier,
            digest: frontier_digest,
        },
    );
    acknowledge_relay(&mut relay, "ack-everything", frontier);
    assert_eq!(sealed(temp.path()), 0, "collection kept sealed segments");

    let response = deferred.finish();
    let RelayResponseBody::Error { error } = &response.body else {
        panic!("a page read from a collected journal must not be served: {response:?}");
    };
    assert!(
        error.retryable,
        "a collected journal is retryable, not a controller fault: {error:?}"
    );
    assert_ne!(
        relay.journal_generation(),
        generation,
        "collection must mark captured replay plans stale"
    );
}

/// A replay page is assembled from files the relay lock no longer guards,
/// so a span whose file was pruned under the reader must fail the read.
/// Silently contributing nothing would answer a catch-up with a page that
/// claims to reach the frontier while carrying none of the events.
#[test]
fn a_deferred_attach_never_serves_a_torn_page_after_its_segments_are_pruned() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    record_paged_history(&mut relay, 80);
    let frontier = relay.latest_ordinal();

    let deferred = relay
        .take_deferred_attach(&attach_envelope(&relay, "catch-up", 0))
        .expect("an attach is deferred off the relay lock");
    for entry in fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR)).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }

    let response = deferred.finish();
    match response.body {
        RelayResponseBody::Error { error } => assert!(
            error.retryable,
            "a pruned segment is retryable, not a controller fault: {error:?}"
        ),
        RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    events,
                    through_ordinal,
                    ..
                },
        } => panic!(
            "served {} events but claimed to reach event {through_ordinal} of {frontier}",
            events.len()
        ),
        other => panic!("unexpected attach response: {other:?}"),
    }
}

#[test]
fn a_deferred_attach_refuses_a_page_from_a_resealed_segment() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    // Fill the active segment without crossing its seal threshold.
    record_paged_history(&mut relay, 8);
    let deferred = relay
        .take_deferred_attach(&attach_envelope(&relay, "catch-up", 0))
        .expect("an attach is deferred off the relay lock");
    let generation = deferred.journal_generation();

    // The next append seals the active segment, moving every event the
    // plan expects to find in `active.jsonl` into a compressed file.
    record_paged_history(&mut relay, 12);
    assert_ne!(relay.journal_generation(), generation);

    let response = deferred.finish();
    let RelayResponseBody::Error { error } = &response.body else {
        panic!("a page read from a resealed segment must not be served: {response:?}");
    };
    assert!(
        error.retryable,
        "a resealed segment is retryable: {error:?}"
    );
}

#[test]
fn relay_runs_queued_prompts_in_order_without_a_controller() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "command-one", prompt("one"));
    submit_relay(&mut relay, "command-two", prompt("two"));
    submit_relay(&mut relay, "command-three", prompt("three"));

    let first = relay.claim_pending_commands(true).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].command_id, "command-one");
    relay
        .record_command_completed(
            "command-one",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    let second = relay.claim_pending_commands(true).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].command_id, "command-two");

    // A relay/process restart interrupts only the command actually handed
    // to ACP and then continues the durable offline queue.
    drop(relay);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let third = relay.claim_pending_commands(true).unwrap();
    assert_eq!(third.len(), 1);
    assert_eq!(third[0].command_id, "command-three");
    assert!(retained_events(&relay).iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandInterrupted { command_id, .. }
            if command_id == "command-two"
    )));
}

#[test]
fn duplicate_image_command_is_idempotent_after_attachment_loss() {
    let temp = tempfile::tempdir().unwrap();
    let store = mj_core::attachment::AttachmentStore::worker(temp.path());
    let bytes = b"\x89PNG\r\n\x1a\nverified-image".to_vec();
    let reference =
        mj_core::attachment::AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
    store.install(&reference, &bytes).unwrap();
    let command = RelayCommand::Prompt {
        prompt: vec![reference.content_block()],
    };
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let ordinal = submit_relay(&mut relay, "image-command", command.clone());
    fs::remove_file(store.root().join(&reference.sha256)).unwrap();
    let latest_before_retry = relay.latest_ordinal();

    let response = relay.handle(relay_request(
        "image-command-retry",
        RelayRequest::Submit {
            command_id: "image-command".into(),
            command,
        },
    ));
    assert!(matches!(
        response.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted {
                command_id,
                ordinal: accepted_ordinal,
            }
        } if command_id == "image-command" && accepted_ordinal == ordinal
    ));
    assert_eq!(relay.latest_ordinal(), latest_before_retry);
}

fn successful_shell(command: &str, stdout: &str) -> UserShellResult {
    UserShellResult {
        command: command.to_owned(),
        stdout: stdout.to_owned(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        exit_code: Some(0),
        signal: None,
        duration_ms: 12,
        status: UserShellStatus::Exited,
        error: None,
    }
}

#[test]
fn shell_runs_during_an_active_turn_and_barriers_the_later_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "prompt-command-1", prompt("first"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "prompt-command-1"
    );
    submit_relay(
        &mut relay,
        "shell-command-01",
        RelayCommand::RunUserShell {
            command: "printf ready".into(),
        },
    );
    submit_relay(&mut relay, "prompt-command-2", prompt("after shell"));

    let shell = relay.claim_pending_user_shell_commands_up_to(4).unwrap();
    assert_eq!(shell[0].command_id, "shell-command-01");
    relay
        .record_command_completed(
            "prompt-command-1",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    relay
        .record_command_completed(
            "shell-command-01",
            RelayCommandOutcome::UserShell {
                result: successful_shell("printf ready", "ready"),
            },
        )
        .unwrap();
    let prompt = relay.claim_pending_commands(true).unwrap();
    assert_eq!(prompt[0].command_id, "prompt-command-2");
    assert!(
        prompt[0]
            .hidden_prompt_context
            .as_deref()
            .is_some_and(
                |context| context.contains("<user_shell_command>") && context.contains("ready")
            )
    );
}

#[test]
fn a_prompt_accepted_before_a_shell_keeps_priority_and_does_not_consume_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "prompt-command-1", prompt("first"));
    relay.claim_pending_commands(true).unwrap();
    submit_relay(&mut relay, "prompt-command-2", prompt("already queued"));
    submit_relay(
        &mut relay,
        "shell-command-01",
        RelayCommand::RunUserShell {
            command: "printf later".into(),
        },
    );
    relay.claim_pending_user_shell_commands_up_to(4).unwrap();
    relay
        .record_command_completed(
            "prompt-command-1",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();

    let prompt = relay.claim_pending_commands(true).unwrap();
    assert_eq!(prompt[0].command_id, "prompt-command-2");
    assert!(prompt[0].hidden_prompt_context.is_none());
}

#[test]
fn a_shell_cancelled_before_launch_still_reaches_the_next_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "shell-command-01",
        RelayCommand::RunUserShell {
            command: "sleep 60".into(),
        },
    );
    submit_relay(
        &mut relay,
        "cancel-shell-01",
        RelayCommand::CancelUserShell {
            shell_command_id: "shell-command-01".into(),
        },
    );

    let claimed = relay.claim_pending_user_shell_commands_up_to(4).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "cancel-shell-01");
    relay
        .record_command_interrupted(
            "shell-command-01",
            "shell command was cancelled before it started",
        )
        .unwrap();
    relay
        .record_command_completed("cancel-shell-01", RelayCommandOutcome::UserShellCancelled)
        .unwrap();

    submit_relay(&mut relay, "prompt-command-1", prompt("what happened?"));
    let prompt = relay.claim_pending_commands(true).unwrap();
    assert!(
        prompt[0]
            .hidden_prompt_context
            .as_deref()
            .is_some_and(|context| {
                context.contains("status: interrupted")
                    && context.contains("cancelled before it started")
            })
    );
}

#[test]
fn claims_wait_for_the_current_acp_session_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "wait-for-config", prompt("later"));

    assert!(relay.claim_pending_commands(false).unwrap().is_empty());
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "wait-for-config");
}

#[test]
fn checkpoint_barrier_pauses_offline_prompt_promotion_until_release() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "barrier-command",
        RelayCommand::BeginCheckpoint {
            reason: Some("test".into()),
        },
    );
    submit_relay(&mut relay, "after-barrier", prompt("later"));
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "barrier-command");
    relay.record_checkpoint_ready("barrier-command").unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    assert!(relay.operational_state().active_prompt.is_none());

    submit_relay(
        &mut relay,
        "release-command",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "barrier-command".into(),
        },
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "after-barrier");
}

#[test]
fn controller_disconnect_cannot_leave_checkpoint_barrier_paused() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "barrier-disconnect",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    submit_relay(&mut relay, "queued-offline", prompt("continue"));
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    relay.record_checkpoint_ready("barrier-disconnect").unwrap();

    let cancelled = relay
        .cancel_checkpoint_barrier_on_disconnect("barrier-disconnect")
        .unwrap();
    assert!(cancelled.is_some());
    assert!(relay.operational_state().checkpoint_barrier.is_none());
    let next = relay.claim_pending_commands(true).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].command_id, "queued-offline");
}

/// The controller releases dispatch once the archive exists on the target,
/// long before that archive is installed. Journal history must stay put
/// until an installed archive covers it.
#[test]
fn releasing_a_checkpoint_resumes_dispatch_without_moving_the_recovery_floor() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let ready = ready_checkpoint(&mut relay, "released-barrier");
    submit_relay(&mut relay, "queued-during-release", prompt("later"));
    attach_relay(&mut relay, "attach-release", 0);
    acknowledge_relay(&mut relay, "ack-release", ready.ordinal);
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    let floor_before = relay.snapshot.recovery_floor_ordinal;
    let retained_before = relay.snapshot.retained_through();

    submit_relay(
        &mut relay,
        "release-command",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "released-barrier".into(),
        },
    );

    assert!(relay.operational_state().checkpoint_barrier.is_none());
    assert!(relay.operational_state().checkpoint_ready.is_none());
    assert_eq!(relay.snapshot.recovery_floor_ordinal, floor_before);
    assert_eq!(relay.snapshot.retained_through(), retained_before);
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "queued-during-release");
    // The released barrier is terminal, so the controller connection that
    // opened it can drop without cancelling anything.
    assert!(
        relay
            .cancel_checkpoint_barrier_on_disconnect("released-barrier")
            .unwrap()
            .is_none()
    );
}

#[test]
fn releasing_a_checkpoint_requires_that_exact_ready_barrier() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let missing = submit_release(&mut relay, "release-without-barrier", "no-such-barrier");
    assert!(matches!(
        missing.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));

    submit_relay(
        &mut relay,
        "unready-barrier",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    let unready = submit_release(&mut relay, "release-unready", "unready-barrier");
    assert!(matches!(
        unready.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));

    relay.record_checkpoint_ready("unready-barrier").unwrap();
    let wrong = submit_release(&mut relay, "release-wrong", "another-barrier");
    assert!(matches!(
        wrong.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));
    assert_eq!(
        relay.operational_state().checkpoint_barrier.as_deref(),
        Some("unready-barrier")
    );
}

/// Installing the archive is what earns the journal release, and the
/// recovery floor is how the relay records it.
#[test]
fn advancing_the_recovery_floor_releases_history_only_forward_and_on_chain() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let ready = ready_checkpoint(&mut relay, "installed-barrier");
    submit_relay(
        &mut relay,
        "release-installed",
        RelayCommand::ReleaseCheckpoint {
            barrier_command_id: "installed-barrier".into(),
        },
    );
    // Acknowledge past the ready cursor so the recovery floor alone decides
    // what the relay retains.
    attach_relay(&mut relay, "attach-floor", 0);
    let acknowledged = relay.latest_ordinal();
    acknowledge_relay(&mut relay, "ack-floor", acknowledged);
    assert_eq!(relay.snapshot.retained_through(), 0);

    let mismatched = submit_floor(
        &mut relay,
        "floor-wrong-digest",
        RelayCursor {
            ordinal: ready.ordinal,
            digest: "b".repeat(64),
        },
    );
    assert!(matches!(
        mismatched.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));
    let beyond_frontier = RelayCursor {
        ordinal: relay.latest_ordinal() + 1,
        digest: relay.snapshot.latest_digest.clone(),
    };
    let ahead = submit_floor(&mut relay, "floor-ahead", beyond_frontier);
    assert!(matches!(
        ahead.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));
    assert_eq!(relay.snapshot.recovery_floor_ordinal, 0);

    submit_relay(
        &mut relay,
        "floor-installed",
        RelayCommand::AdvanceRecoveryFloor {
            through: ready.clone(),
        },
    );
    assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
    assert_eq!(relay.snapshot.recovery_floor_digest, ready.digest);
    assert_eq!(relay.snapshot.retained_through(), ready.ordinal);

    let backwards = submit_floor(
        &mut relay,
        "floor-backwards",
        RelayCursor {
            ordinal: 0,
            digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        },
    );
    assert!(matches!(
        backwards.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));
    assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
}

/// The legacy one-step completion is unchanged: it both resumes dispatch
/// and advances the recovery floor.
#[test]
fn completing_a_checkpoint_still_resumes_dispatch_and_advances_the_floor() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let ready = ready_checkpoint(&mut relay, "completed-barrier");
    submit_relay(&mut relay, "queued-during-completion", prompt("later"));
    attach_relay(&mut relay, "attach-completion", 0);
    acknowledge_relay(&mut relay, "ack-completion", ready.ordinal);

    submit_relay(
        &mut relay,
        "complete-command",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "completed-barrier".into(),
        },
    );

    assert!(relay.operational_state().checkpoint_barrier.is_none());
    assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
    assert_eq!(relay.snapshot.retained_through(), ready.ordinal);
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "queued-during-completion");
}

#[test]
fn checkpoint_barriers_are_serialized_and_only_exact_completion_releases_them() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "first-barrier",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    submit_relay(
        &mut relay,
        "second-barrier",
        RelayCommand::BeginCheckpoint { reason: None },
    );

    let first = relay.claim_pending_commands(true).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].command_id, "first-barrier");
    relay.record_checkpoint_ready("first-barrier").unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    let wrong = relay.handle(relay_request(
        "wrong-completion",
        RelayRequest::Submit {
            command_id: "wrong-complete-command".into(),
            command: RelayCommand::CompleteCheckpoint {
                barrier_command_id: "second-barrier".into(),
            },
        },
    ));
    assert!(matches!(
        wrong.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));

    submit_relay(
        &mut relay,
        "complete-first",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "first-barrier".into(),
        },
    );
    let second = relay.claim_pending_commands(true).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].command_id, "second-barrier");
}

#[test]
fn checkpoint_waits_for_earlier_queued_control_and_freezes_later_control() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "config-before",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "before".into(),
        },
    );
    submit_relay(
        &mut relay,
        "control-barrier",
        RelayCommand::BeginCheckpoint { reason: None },
    );

    let control = relay.claim_pending_commands(true).unwrap();
    assert_eq!(control.len(), 1);
    assert_eq!(control[0].command_id, "config-before");
    relay
        .record_command_completed("config-before", RelayCommandOutcome::Configured)
        .unwrap();
    assert_eq!(relay.operational_state().config["model"], "before");

    let barrier = relay.claim_pending_commands(true).unwrap();
    assert_eq!(barrier.len(), 1);
    assert_eq!(barrier[0].command_id, "control-barrier");
    relay.record_checkpoint_ready("control-barrier").unwrap();
    submit_relay(
        &mut relay,
        "config-after",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "after".into(),
        },
    );
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    submit_relay(
        &mut relay,
        "complete-control-barrier",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "control-barrier".into(),
        },
    );
    let later = relay.claim_pending_commands(true).unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].command_id, "config-after");
}

#[test]
fn a_recorded_notice_becomes_one_verbatim_system_line() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let text = "This session moved from /home/dev/project into a container.";

    submit_relay(
        &mut relay,
        "resume-notice-1",
        RelayCommand::RecordNotice { text: text.into() },
    );

    // A notice never reaches ACP: it completes inside the relay.
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    let mut session = mj_core::state::MaterializedSession::empty(SESSION);
    for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
        let projected = mj_transcript::projection::project_relay_event(&session, &event).unwrap();
        mj_transcript::projection::apply_committed_projection_event(
            &mut session,
            &event,
            projected.mutation,
        )
        .unwrap();
    }

    let notices = session
        .transcript
        .iter()
        .filter_map(|item| match &item.body {
            mj_core::state::TranscriptBody::System { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices, vec![text.to_owned()]);
}

#[test]
fn a_repeated_notice_append_still_leaves_one_conversation_line() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let text = "The working tree moved while this session was stopped.";
    submit_relay(
        &mut relay,
        "resume-notice-1",
        RelayCommand::RecordNotice { text: text.into() },
    );
    // Stand in for a retry that re-appended the notice after a transient
    // persistence failure reported a durable append as unfinished.
    relay
        .append_relay_event(
            Some("resume-notice-1"),
            RelayObservation::Notice {
                message: text.into(),
            },
        )
        .unwrap();

    let mut session = mj_core::state::MaterializedSession::empty(SESSION);
    for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
        let projected = mj_transcript::projection::project_relay_event(&session, &event).unwrap();
        mj_transcript::projection::apply_committed_projection_event(
            &mut session,
            &event,
            projected.mutation,
        )
        .unwrap();
    }

    assert_eq!(session.transcript.len(), 1);
}

#[test]
fn codex_replies_preserve_the_user_turn_and_only_autonomous_work_adds_a_notice() {
    use mj_core::state::{MaterializedExecutionState, MaterializedSession};
    use mj_core::transcript::HARNESS_TURN_ITEM_PREFIX;

    for goal in [
        serde_json::Value::Null,
        serde_json::json!({"objective":"finish","status":"active","createdAt":1}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(root.path(), SESSION, "test").unwrap();
        relay.set_harness_turn_policy(HarnessTurnPolicy::CodexAdapter);
        let metadata = |status, turn_id| {
            serde_json::from_value::<SessionUpdate>(serde_json::json!({
                "sessionUpdate":"session_info_update",
                "_meta": {
                    "goal":goal,
                    "execution":{"version":1,"status":status,"turnId":turn_id}
                }
            }))
            .unwrap()
        };
        let project = |relay: &DurableRelay| {
            let mut session = MaterializedSession::empty(SESSION);
            for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
                let projected =
                    mj_transcript::projection::project_relay_event(&session, &event).unwrap();
                mj_transcript::projection::apply_committed_projection_event(
                    &mut session,
                    &event,
                    projected.mutation,
                )
                .unwrap();
            }
            session
        };
        let marker_count = |session: &MaterializedSession| {
            session
                .transcript
                .iter()
                .filter(|item| item.stable_id.starts_with(HARNESS_TURN_ITEM_PREFIX))
                .count()
        };
        let command_id = "00000000000000000000000000000001";
        submit_relay(
            &mut relay,
            command_id,
            prompt("Are those problems captured in a ticket?"),
        );
        relay.claim_pending_commands(true).unwrap();
        let before = project(&relay);
        assert!(before.active_turn.is_some());
        for _ in 0..2 {
            relay
                .record_session_update(metadata("running", "reply"))
                .unwrap();
        }
        let replying = project(&relay);
        assert_eq!(marker_count(&replying), 0);
        assert_eq!(replying.active_turn, before.active_turn);
        assert_eq!(replying.execution, before.execution);
        assert_eq!(replying.transcript, before.transcript);

        // Native execution can outlast the ACP prompt result. Suppressing
        // the notice must not settle that work or permit replacement.
        relay
            .record_command_completed(
                command_id,
                RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
        let running = project(&relay);
        assert!(running.active_turn.is_none());
        assert!(matches!(
            running.execution,
            MaterializedExecutionState::Running { .. }
        ));
        assert_eq!(marker_count(&running), 0);
        let operational = relay.operational_state();
        assert!(!operational.safe_to_replace(mj_core::config::HarnessKind::Codex));
        assert!(!operational.safe_for_checkpoint(mj_core::config::HarnessKind::Codex));
        relay
            .record_session_update(metadata("idle", "reply"))
            .unwrap();
        assert_eq!(project(&relay).execution, MaterializedExecutionState::Idle);

        for _ in 0..2 {
            relay
                .record_session_update(metadata("running", "autonomous"))
                .unwrap();
        }
        assert_eq!(marker_count(&project(&relay)), 1);
        relay
            .record_session_update(metadata("idle", "autonomous"))
            .unwrap();
        let settled = project(&relay);
        assert_eq!(marker_count(&settled), 1);
        assert_eq!(settled.execution, MaterializedExecutionState::Idle);
        assert_eq!(settled.transcript, project(&relay).transcript);
    }
}

#[test]
fn codex_goal_turns_block_replacement_after_the_prompt_finishes() {
    use mj_core::config::HarnessKind;
    let root = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(root.path(), SESSION, "test").unwrap();
    relay.set_harness_turn_policy(HarnessTurnPolicy::CodexAdapter);
    let metadata = |meta| {
        serde_json::from_value::<SessionUpdate>(
            serde_json::json!({"sessionUpdate":"session_info_update","_meta":meta}),
        )
        .unwrap()
    };
    assert!(
        !relay
            .operational_state()
            .safe_to_replace(HarnessKind::Codex)
    );
    submit_relay(
        &mut relay,
        "00000000000000000000000000000001",
        prompt("/goal finish"),
    );
    relay.claim_pending_commands(true).unwrap();
    relay.record_session_update(metadata(serde_json::json!({"goal":{"objective":"finish","status":"active","createdAt":1},"execution":{"version":1,"status":"running","turnId":"autonomous"}}))).unwrap();
    relay
        .record_command_completed(
            "00000000000000000000000000000001",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    let state = relay.operational_state();
    assert!(state.active_prompt.is_none());
    assert!(state.harness_turn.is_some());
    assert_eq!(state.execution, RelayExecutionState::Running);
    assert!(!state.safe_to_replace(HarnessKind::Codex));
    assert!(!state.safe_for_checkpoint(HarnessKind::Codex));
    relay
        .record_session_update(metadata(
            serde_json::json!({"execution":{"version":1,"status":"idle","turnId":"autonomous"}}),
        ))
        .unwrap();
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Idle
    );
    assert!(
        !relay
            .operational_state()
            .safe_to_replace(HarnessKind::Codex),
        "between goal turns is not safe to kill"
    );
    drop(relay);
    let mut relay = DurableRelay::open(root.path(), SESSION, "test").unwrap();
    relay
        .record_observation(RelayObservation::SessionRestarted)
        .unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "native".into(),
            resumed: true,
            native_continuity_lost: false,
        })
        .unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    assert!(relay.operational_state().goal.active());
    assert!(!relay.operational_state().goal.synchronized());
    relay
        .record_session_update(metadata(
            serde_json::json!({"goal":null,"execution":{"version":1,"status":"idle"}}),
        ))
        .unwrap();
    assert!(
        relay
            .operational_state()
            .safe_to_replace(HarnessKind::Codex)
    );
}

/// Build a relay that models the turns Claude Code starts on its own.
fn claude_relay(root: &std::path::Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "1.0.0").unwrap();
    relay.set_harness_turn_policy(HarnessTurnPolicy::ClaudeAdapter);
    relay.set_background_work_policy(BackgroundWorkPolicy::ClaudeTasks);
    relay
}

fn tool_call_update() -> SessionUpdate {
    SessionUpdate::ToolCall(agent_client_protocol::schema::v1::ToolCall::new(
        "call-1", "Bash",
    ))
}

/// The `usage_update` the Claude adapter sends when an SDK cycle ends and
/// the cycle produced assistant usage.
fn origin_marker(origin: &str) -> SessionUpdate {
    let mut usage = agent_client_protocol::schema::v1::UsageUpdate::new(10, 200);
    usage.meta = Some(
        serde_json::from_value(serde_json::json!({
            "_claude/origin": {"kind": origin},
        }))
        .unwrap(),
    );
    SessionUpdate::UsageUpdate(usage)
}

/// The SDK `result` that ends a Claude Code model cycle.
fn cycle_result(origin: &str) -> mj_core::acp::ClaudeTurnResult {
    mj_core::acp::ClaudeTurnResult::from_sdk_message(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false, "num_turns": 1,
        "stop_reason": "end_turn", "result": "done",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "origin": {"kind": origin},
    }))
    .unwrap()
    .unwrap()
}

fn observations(relay: &DurableRelay) -> Vec<RelayObservation> {
    relay
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap()
        .into_iter()
        .map(|event| event.observation)
        .collect()
}

#[test]
fn agent_output_at_idle_opens_a_harness_turn_and_its_cycle_result_settles_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);

    relay.record_session_update(tool_call_update()).unwrap();

    let running = relay.operational_state();
    assert_eq!(running.execution, RelayExecutionState::Running);
    assert!(running.active_prompt.is_none(), "no prompt is in flight");
    let turn = running.harness_turn.expect("a harness turn is open");
    assert!(turn.started_at_ms > 0);
    assert_eq!(running.last_harness_turn_started_ordinal, Some(1));
    assert!(matches!(
        observations(&relay).as_slice(),
        [
            RelayObservation::HarnessTurnStarted { .. },
            RelayObservation::SessionUpdate { .. }
        ]
    ));

    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();

    let settled = relay.operational_state();
    assert_eq!(settled.execution, RelayExecutionState::Idle);
    assert!(settled.harness_turn.is_none());
    assert_eq!(
        settled.foreground_tool_started_at_ms, None,
        "the confirmed turn boundary clears any tool status the adapter left open"
    );
    assert_eq!(
        settled.last_harness_turn_started_ordinal,
        Some(1),
        "the started ordinal only moves forward, so a checkpoint can compare against it"
    );
    assert!(matches!(
        observations(&relay).last(),
        Some(RelayObservation::HarnessTurnSettled { origin, .. })
            if origin.as_deref() == Some("task-notification")
    ));
    let (_, evidence, _) = relay
        .pending_replied_verdict()
        .expect("settled harness turn is classified");
    assert_eq!(
        evidence.phase,
        mj_core::activity::verdict::TurnPhase::Replied
    );
    assert!(relay.pending_replied_verdict().is_none());
}

/// The adapter's origin marker is left out when a cycle produced no
/// assistant usage, so it cannot be what ends a Claude turn. The result can:
/// Claude Code sends one for every cycle.
#[test]
fn a_claude_harness_turn_settles_on_its_result_and_not_on_the_origin_marker() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());

    relay
        .record_session_update(agent_text_chunk("The build finished."))
        .unwrap();
    relay
        .record_session_update(origin_marker("task-notification"))
        .unwrap();
    let state = relay.operational_state();
    assert!(
        state.harness_turn.is_some(),
        "the marker alone settles nothing"
    );
    assert_eq!(state.execution, RelayExecutionState::Running);

    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    let state = relay.operational_state();
    assert!(state.harness_turn.is_none());
    assert_eq!(state.execution, RelayExecutionState::Idle);

    // A cycle that streams text but sends no marker still ends.
    relay
        .record_session_update(agent_text_chunk("One more thing."))
        .unwrap();
    assert!(relay.operational_state().harness_turn.is_some());
    relay.claude_turn_result(&cycle_result("human")).unwrap();
    assert!(relay.operational_state().harness_turn.is_none());
    assert!(matches!(
        observations(&relay).last(),
        Some(RelayObservation::HarnessTurnSettled { origin, prompt_in_flight: false })
            if origin.as_deref() == Some("human")
    ));

    // A result with no turn open records nothing.
    let before = relay.operational_state().latest_ordinal;
    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    assert_eq!(relay.operational_state().latest_ordinal, before);
}

/// Stop while Claude Code works on its own after a background task sends
/// `session/cancel`; the interrupted cycle's result then ends the turn. With
/// nothing running, or during a Codex goal turn, Stop is still refused.
#[test]
fn stop_during_a_claude_harness_turn_is_dispatched_and_the_interrupted_result_ends_it() {
    let refused = |relay: &mut DurableRelay, command_id: &str| {
        let response = relay.handle(relay_request(
            &format!("request-{command_id}"),
            RelayRequest::Submit {
                command_id: command_id.to_owned(),
                command: RelayCommand::Cancel,
            },
        ));
        match response.body {
            RelayResponseBody::Error { error } => error.message,
            body => panic!("Stop must be refused: {body:?}"),
        }
    };
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    assert_eq!(
        refused(&mut relay, "cancel-idle"),
        "there is no active prompt to cancel"
    );

    relay
        .record_session_update(agent_text_chunk("Summarizing the agent's findings"))
        .unwrap();
    assert!(relay.operational_state().harness_turn.is_some());
    submit_relay(&mut relay, "cancel-harness-turn", RelayCommand::Cancel);
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "cancel-harness-turn");
    relay
        .record_command_completed("cancel-harness-turn", RelayCommandOutcome::Cancelled)
        .unwrap();
    assert!(
        relay.operational_state().harness_turn.is_some(),
        "the turn runs until Claude Code reports the interrupted cycle"
    );
    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    let state = relay.operational_state();
    assert!(state.harness_turn.is_none());
    assert_eq!(state.execution, RelayExecutionState::Idle);

    let codex = tempfile::tempdir().unwrap();
    let mut codex = DurableRelay::open(codex.path(), SESSION, "1.0.0").unwrap();
    codex.set_harness_turn_policy(HarnessTurnPolicy::CodexAdapter);
    codex
        .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
        .unwrap();
    assert!(codex.operational_state().harness_turn.is_some());
    assert_eq!(
        refused(&mut codex, "cancel-codex"),
        "the agent is working on its own after a background task; there is no prompt to cancel"
    );
}

/// What the Claude adapter (claude-agent-acp 0.81.0, `dist/session-mode.js`,
/// `publishFallbackWarning`) sends as agent text while it answers a switch to
/// a model without Auto mode. Recorded in launch re-verification R4 (session
/// 7214842e…, transcript item 30) after `/model haiku` on a session with no
/// prompt yet.
const CLAUDE_AUTO_MODE_FALLBACK_TEXT: &str = "**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.";

/// R4-2: the adapter's answer to a model change is not a model cycle, so no
/// result ever follows it. A turn opened for that text stayed Running for
/// minutes and nothing could stop it.
#[test]
fn text_that_answers_a_model_change_opens_no_harness_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    submit_relay(&mut relay, "model-haiku", set_config("model", "haiku"));
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "model-haiku");

    relay
        .record_session_update(agent_text_chunk(CLAUDE_AUTO_MODE_FALLBACK_TEXT))
        .unwrap();
    let state = relay.operational_state();
    assert!(
        state.harness_turn.is_none(),
        "the answer to a configuration request is not a turn"
    );
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert!(
        observations(&relay).iter().any(|observation| matches!(
            observation,
            RelayObservation::SessionUpdate { update }
                if agent_chunk_text(update) == Some(CLAUDE_AUTO_MODE_FALLBACK_TEXT)
        )),
        "the notice still reaches the conversation"
    );

    relay
        .record_observation(RelayObservation::ConfigurationUpdated {
            key: "model".into(),
            value: "haiku".into(),
        })
        .unwrap();
    relay
        .record_command_completed("model-haiku", RelayCommandOutcome::Configured)
        .unwrap();
    let state = relay.operational_state();
    assert!(state.harness_turn.is_none());
    assert_eq!(state.execution, RelayExecutionState::Idle);

    // Text with no request in flight is still Claude Code working on its own.
    relay
        .record_session_update(agent_text_chunk("The build finished."))
        .unwrap();
    assert!(relay.operational_state().harness_turn.is_some());
}

/// R4-2: a stop of a turn Claude Code never ran is answered by nothing, so
/// the relay ends that turn itself once the stop has had time to be answered.
/// A real cycle's result still ends the turn first.
#[test]
fn a_stop_the_harness_never_answers_ends_the_self_started_turn() {
    for command in [RelayCommand::Cancel, RelayCommand::CancelTurn] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .record_session_update(agent_text_chunk(CLAUDE_AUTO_MODE_FALLBACK_TEXT))
            .unwrap();
        let turn = relay
            .operational_state()
            .harness_turn
            .expect("text at idle opens a turn");
        assert!(relay.unanswered_harness_turn_stop().is_none());

        submit_relay(&mut relay, "stop-phantom", command.clone());
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        relay
            .record_command_completed("stop-phantom", RelayCommandOutcome::Cancelled)
            .unwrap();
        let first_ordinal = relay
            .unanswered_harness_turn_stop()
            .expect("the applied stop waits for the harness to answer it");
        assert_eq!(
            relay.operational_state().harness_turn,
            Some(turn),
            "the turn stays open while the harness may still answer"
        );

        assert!(
            relay
                .end_unanswered_harness_turn_stop(first_ordinal)
                .unwrap(),
            "{command:?}"
        );
        let state = relay.operational_state();
        assert!(state.harness_turn.is_none());
        assert_eq!(state.execution, RelayExecutionState::Idle);
        assert!(matches!(
            observations(&relay).last(),
            Some(RelayObservation::HarnessTurnSettled {
                prompt_in_flight: false,
                ..
            })
        ));
        assert!(relay.unanswered_harness_turn_stop().is_none());
    }

    // The interrupted cycle's result arrives in time: nothing is left to end.
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay
        .record_session_update(agent_text_chunk("Summarizing"))
        .unwrap();
    submit_relay(&mut relay, "stop-real", RelayCommand::CancelTurn);
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed("stop-real", RelayCommandOutcome::Cancelled)
        .unwrap();
    let first_ordinal = relay.unanswered_harness_turn_stop().unwrap();
    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    assert!(relay.unanswered_harness_turn_stop().is_none());
    let before = relay.operational_state().latest_ordinal;
    assert!(
        !relay
            .end_unanswered_harness_turn_stop(first_ordinal)
            .unwrap()
    );
    assert_eq!(relay.operational_state().latest_ordinal, before);
}

#[test]
fn a_harness_turn_holds_the_checkpoint_barrier_until_it_settles() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();

    submit_relay(
        &mut relay,
        "barrier-command",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    assert!(
        relay.claim_pending_commands(true).unwrap().is_empty(),
        "a turn the harness started on its own must keep the barrier queued"
    );

    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();

    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "barrier-command");
}

#[test]
fn a_prompt_queued_during_a_harness_turn_dispatches_at_once() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();

    submit_relay(&mut relay, "typed-mid-turn", prompt("answer this too"));
    let claimed = relay.claim_pending_commands(true).unwrap();

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "typed-mid-turn");
    assert!(
        observations(&relay).iter().any(|observation| matches!(
            observation,
            RelayObservation::CommandStarted { command_id, .. } if command_id == "typed-mid-turn"
        )),
        "the prompt starts while the harness turn is still open"
    );
    assert!(
        relay.operational_state().harness_turn.is_some(),
        "dispatching a prompt does not end the turn the harness started"
    );

    // Barrier priority over queued prompts is an invariant: a pending
    // barrier still freezes a prompt typed after it.
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();
    submit_relay(
        &mut relay,
        "barrier-command",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    submit_relay(&mut relay, "typed-after-barrier", prompt("wait for me"));

    assert!(
        relay.claim_pending_commands(true).unwrap().is_empty(),
        "a pending barrier still outranks a prompt typed during a harness turn"
    );
}

#[test]
fn a_prompt_result_settles_a_lingering_harness_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();
    submit_relay(&mut relay, "next-prompt", prompt("carry on"));
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);

    relay
        .record_command_completed(
            "next-prompt",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();

    let state = relay.operational_state();
    assert!(
        state.harness_turn.is_none(),
        "a prompt result means the SDK reached a turn boundary"
    );
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert_eq!(
        state.foreground_tool_started_at_ms, None,
        "the prompt result outranks a stale pending tool status"
    );
}

#[test]
fn a_restart_clears_a_harness_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();

    relay
        .record_observation(RelayObservation::SessionRestarted)
        .unwrap();

    let state = relay.operational_state();
    assert!(state.harness_turn.is_none());
    assert_eq!(state.execution, RelayExecutionState::Idle);
}

/// A Codex `exec_command` card. The adapter reports the result under
/// `rawOutput`, with `exit_code` null while the process is still running.
fn exec_card(
    tool_call_id: &'static str,
    command: &[&str],
    exit_code: Option<i64>,
) -> SessionUpdate {
    let mut call = agent_client_protocol::schema::v1::ToolCall::new(tool_call_id, "shell");
    call.kind = agent_client_protocol::schema::v1::ToolKind::Execute;
    call.status = agent_client_protocol::schema::v1::ToolCallStatus::Completed;
    call.raw_input = Some(serde_json::json!({ "command": command }));
    call.raw_output = Some(serde_json::json!({
        "output": "",
        "exit_code": exit_code,
    }));
    SessionUpdate::ToolCall(call)
}

fn exec_card_update(tool_call_id: &'static str, exit_code: Option<i64>) -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        tool_call_id,
        ToolCallUpdateFields::new()
            .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
            .raw_output(serde_json::json!({
                "output": "",
                "exit_code": exit_code,
            })),
    ))
}

fn kimi_background_agent_card() -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

    let mut call = ToolCall::new(
        "1:tool_agent",
        "Launching background coder agent: Fix memory use",
    );
    call.status = ToolCallStatus::Completed;
    call.raw_input = Some(serde_json::json!({
        "description": "Fix memory use",
        "prompt": "Fix the issue",
        "run_in_background": true
    }));
    call.raw_output = Some(serde_json::Value::String(
        "task_id: agent-deadbeef\nstatus: running\nagent_id: agent-1".into(),
    ));
    SessionUpdate::ToolCall(call)
}

fn kimi_background_agent_card_with_run_in_background_only() -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

    let mut call = ToolCall::new("run-only", "agent");
    call.status = ToolCallStatus::Completed;
    call.raw_input = Some(serde_json::json!({
        "description": "Run-only agent",
        "run_in_background": true
    }));
    SessionUpdate::ToolCall(call)
}

#[test]
fn kimi_task_queries_reconcile_by_native_identity_in_either_event_order() {
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

    for native_first in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), "kimi-queries", "test").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        let query = |call_id: &'static str, title: &'static str| {
            let mut call = ToolCall::new(call_id, title);
            call.status = ToolCallStatus::Completed;
            call.raw_input = Some(serde_json::json!({"task_id": "bash-tlqj0v63"}));
            call.raw_output = Some(serde_json::json!({
                "output": "retrieval_status: not_ready\ntask_id: bash-tlqj0v63\nstatus: running\nparent_tool_call_id: tool-launcher\n"
            }));
            SessionUpdate::ToolCall(call)
        };
        let native = crate::acp::KimiBackgroundTask {
            is_agent: false,
            task_id: "bash-tlqj0v63".into(),
            description: "validation".into(),
            started_at_ms: 1_000,
            parent_tool_call_id: Some("tool-launcher".into()),
        };
        let tools = BTreeSet::from(["tool-launcher".into()]);
        let tasks = BTreeSet::from(["bash-tlqj0v63".into()]);
        if native_first {
            relay
                .kimi_background_tasks_changed(vec![native.clone()], tools.clone(), tasks.clone())
                .unwrap();
        }
        for (id, title) in [("0:query-one", "TaskOutput"), ("0:query-two", "WaitFor")] {
            relay.record_session_update(query(id, title)).unwrap();
            assert_eq!(relay.operational_state().background_commands.len(), 1);
        }
        relay
            .kimi_background_tasks_changed(vec![native], tools.clone(), tasks.clone())
            .unwrap();
        assert_eq!(relay.operational_state().background_commands.len(), 1);
        relay
            .kimi_background_tasks_changed(Vec::new(), tools, tasks)
            .unwrap();
        assert!(relay.operational_state().background_commands.is_empty());
        relay
            .record_session_update(query("0:late-query", "TaskOutput"))
            .unwrap();
        assert!(relay.operational_state().background_commands.is_empty());
        assert!(relay.operational_state().is_quiet());
    }
}

fn kimi_background_agent_card_with_running_task_id_only() -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

    let mut call = ToolCall::new("task-only", "agent");
    call.status = ToolCallStatus::Completed;
    call.raw_input = Some(serde_json::json!({
        "description": "Task-only agent"
    }));
    call.raw_output = Some(serde_json::Value::String(
        "task_id: agent-task-only\nstatus: running".into(),
    ));
    SessionUpdate::ToolCall(call)
}

/// Kimi's `Bash` launcher card with `run_in_background`, optionally
/// embedding the hosted terminal the detached shell runs in.
fn kimi_background_shell_card(
    tool_call_id: &'static str,
    terminal_id: Option<&str>,
) -> SessionUpdate {
    use agent_client_protocol::schema::v1::{Terminal, ToolCall, ToolCallContent, ToolCallStatus};

    let mut call = ToolCall::new(tool_call_id, "Bash");
    call.status = ToolCallStatus::Completed;
    call.raw_input = Some(serde_json::json!({
        "command": "cargo build --release",
        "description": "Build release runner",
        "run_in_background": true
    }));
    if let Some(terminal_id) = terminal_id {
        call.content = vec![ToolCallContent::Terminal(Terminal::new(
            terminal_id.to_owned(),
        ))];
    }
    SessionUpdate::ToolCall(call)
}

fn kimi_process_task(parent_tool_call_id: &str) -> crate::acp::KimiBackgroundTask {
    crate::acp::KimiBackgroundTask {
        is_agent: false,
        task_id: "bash-r5ae".into(),
        description: "Build release runner".into(),
        started_at_ms: 2_000,
        parent_tool_call_id: Some(parent_tool_call_id.into()),
    }
}

/// Put a prompt of ours in flight so the turn-scoped terminal rule applies.
fn kimi_relay_with_prompt_in_flight(root: &Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    submit_relay(&mut relay, "parent-command", prompt("build it"));
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    relay
}

#[test]
fn kimi_detached_shell_is_listed_once_as_its_hosted_terminal_during_the_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = kimi_relay_with_prompt_in_flight(temp.path());
    relay
        .record_session_update(kimi_background_shell_card("3:tool_bash", Some("term-60")))
        .unwrap();
    relay
        .agent_terminal_started(ActiveAgentTerminal {
            terminal_id: "term-60".into(),
            command: "cargo build --release".into(),
            started_at_ms: 2_000,
        })
        .unwrap();
    relay
        .kimi_background_tasks_changed(
            vec![kimi_process_task("tool_bash")],
            BTreeSet::from(["tool_bash".into()]),
            BTreeSet::new(),
        )
        .unwrap();

    let commands = relay.operational_state().background_commands;
    assert_eq!(
        commands
            .iter()
            .map(|command| command.id.as_str())
            .collect::<Vec<_>>(),
        ["terminal:term-60"],
        "the detached shell is one job, stoppable through its terminal"
    );

    relay.agent_terminal_closed("term-60").unwrap();
    relay
        .kimi_background_tasks_changed(
            Vec::new(),
            BTreeSet::from(["tool_bash".into()]),
            BTreeSet::new(),
        )
        .unwrap();
    assert!(
        relay.operational_state().background_commands.is_empty(),
        "the shell exited, so nothing is left running"
    );
}

#[test]
fn kimi_detached_shell_without_a_bound_terminal_is_listed_as_a_native_task() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = kimi_relay_with_prompt_in_flight(temp.path());
    relay
        .kimi_background_tasks_changed(
            vec![kimi_process_task("tool_bash")],
            BTreeSet::from(["tool_bash".into()]),
            BTreeSet::new(),
        )
        .unwrap();

    let commands = relay.operational_state().background_commands;
    assert_eq!(
        commands
            .iter()
            .map(|command| command.id.as_str())
            .collect::<Vec<_>>(),
        ["kimi:bash-r5ae"],
        "without ACP evidence the native record is all there is"
    );
}

#[test]
fn a_kimi_hosted_terminal_with_no_detachment_evidence_stays_hidden_during_the_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = kimi_relay_with_prompt_in_flight(temp.path());
    relay
        .record_session_update(kimi_background_shell_card("3:tool_wait", None))
        .unwrap();
    relay
        .agent_terminal_started(ActiveAgentTerminal {
            terminal_id: "term-61".into(),
            command: "cargo test".into(),
            started_at_ms: 2_000,
        })
        .unwrap();

    assert!(
        !relay
            .operational_state()
            .background_commands
            .iter()
            .any(|command| command.id == "terminal:term-61"),
        "a terminal the turn may still be waiting on is the turn's own work"
    );
}

#[test]
fn kimi_agent_acp_evidence_tracks_each_background_alternative_independently() {
    for update in [
        kimi_background_agent_card_with_run_in_background_only(),
        kimi_background_agent_card_with_running_task_id_only(),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay.record_session_update(update).unwrap();

        let state = relay.operational_state();
        assert_eq!(state.background_commands.len(), 1);
        assert_eq!(state.background_work_known, Some(false));
        assert!(!state.is_quiet());
    }
}

#[test]
fn unmatched_kimi_provisional_work_survives_empty_native_scan_until_evidence_or_teardown() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
    relay
        .record_session_update(kimi_background_agent_card())
        .unwrap();

    relay
        .kimi_background_tasks_changed(Vec::new(), BTreeSet::new(), BTreeSet::new())
        .unwrap();
    assert_eq!(relay.operational_state().background_commands.len(), 1);

    relay
        .kimi_background_tasks_changed(
            vec![crate::acp::KimiBackgroundTask {
                is_agent: false,
                task_id: "agent-deadbeef".into(),
                description: "Fix memory use".into(),
                started_at_ms: 1_000,
                parent_tool_call_id: Some("tool_agent".into()),
            }],
            BTreeSet::from(["tool_agent".into()]),
            BTreeSet::new(),
        )
        .unwrap();
    let state = relay.operational_state();
    assert_eq!(state.background_commands.len(), 1);
    assert_eq!(state.background_commands[0].id, "kimi:agent-deadbeef");

    for observation in [
        RelayObservation::SessionRestarted,
        RelayObservation::Closing,
        RelayObservation::Closed,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
        relay
            .record_session_update(kimi_background_agent_card())
            .unwrap();
        relay
            .kimi_background_tasks_changed(Vec::new(), BTreeSet::new(), BTreeSet::new())
            .unwrap();
        relay.record_observation(observation).unwrap();
        let state = relay.operational_state();
        assert!(state.background_commands.is_empty());
        assert_eq!(state.background_work_known, Some(false));
    }
}

#[test]
fn kimi_background_agent_survives_its_parent_prompt_until_native_termination() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    submit_relay(&mut relay, "parent-command", prompt("delegate this"));
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);

    relay
        .record_session_update(kimi_background_agent_card())
        .unwrap();
    relay
        .record_command_completed(
            "parent-command",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();

    let provisional = relay.operational_state();
    assert_eq!(provisional.background_commands.len(), 1);
    assert_eq!(provisional.background_work_known, Some(false));
    assert!(!provisional.is_quiet());

    relay
        .kimi_background_tasks_changed(
            vec![crate::acp::KimiBackgroundTask {
                is_agent: false,
                task_id: "agent-deadbeef".into(),
                description: "Fix memory use".into(),
                started_at_ms: 1_000,
                parent_tool_call_id: Some("tool_agent".into()),
            }],
            BTreeSet::from(["tool_agent".into()]),
            BTreeSet::new(),
        )
        .unwrap();
    let running = relay.operational_state();
    assert_eq!(running.background_commands.len(), 1);
    assert_eq!(running.background_work_known, Some(true));
    assert!(!running.is_quiet());

    relay
        .kimi_background_tasks_changed(
            Vec::new(),
            BTreeSet::from(["tool_agent".into()]),
            BTreeSet::new(),
        )
        .unwrap();
    let terminated = relay.operational_state();
    assert!(terminated.background_commands.is_empty());
    assert_eq!(terminated.background_work_known, Some(true));
    assert!(terminated.is_quiet());
}

#[test]
fn kimi_tracker_failure_retains_work_and_blocks_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::KimiTasks);
    relay
        .kimi_background_tasks_changed(
            vec![crate::acp::KimiBackgroundTask {
                is_agent: false,
                task_id: "agent-deadbeef".into(),
                description: "Fix memory use".into(),
                started_at_ms: 1_000,
                parent_tool_call_id: None,
            }],
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .unwrap();

    relay.kimi_background_tasks_unavailable().unwrap();

    let state = relay.operational_state();
    assert_eq!(state.background_commands.len(), 1);
    assert_eq!(state.background_work_known, Some(false));
    assert!(!state.is_quiet());
}

#[test]
fn a_terminal_the_agent_left_running_is_background_work_once_the_turn_ends() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();
    relay
        .agent_terminal_started(ActiveAgentTerminal {
            terminal_id: "terminal-1".into(),
            command: "cargo test".into(),
            started_at_ms: 4_000,
        })
        .unwrap();

    assert!(
        relay.operational_state().background_commands.is_empty(),
        "a terminal is the turn's own work while that turn is still open"
    );

    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();

    assert_eq!(
        relay.operational_state().background_commands,
        vec![BackgroundCommand {
            id: "terminal:terminal-1".into(),
            started_at_ms: 4_000,
            command: "cargo test".into(),
            can_stop: true,
        }],
        "the command outlived the turn that started it"
    );

    relay.agent_terminal_closed("terminal-1").unwrap();
    assert!(
        relay.operational_state().background_commands.is_empty(),
        "the process exited, so there is nothing left running"
    );
}

fn claude_task(task_id: &str, description: &str) -> crate::acp::ClaudeBackgroundTask {
    crate::acp::ClaudeBackgroundTask {
        task_id: task_id.into(),
        description: description.into(),
    }
}

#[test]
fn claude_background_tasks_survive_prompt_boundaries_until_the_level_is_empty() {
    for outcome in ["completed", "rejected", "interrupted"] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        submit_relay(
            &mut relay,
            "review-prompt",
            prompt("start background reviews"),
        );
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        relay.record_session_update(tool_call_update()).unwrap();
        relay
            .claude_background_tasks_changed(vec![
                claude_task("design", "Design review"),
                claude_task("refuter", "Refute findings"),
            ])
            .unwrap();
        match outcome {
            "completed" => relay.record_command_completed(
                "review-prompt",
                RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            ),
            "rejected" => relay.record_command_rejected("review-prompt", "adapter failed"),
            _ => relay.record_command_interrupted("review-prompt", "cancelled"),
        }
        .unwrap();

        let state = relay.operational_state();
        assert_eq!(state.execution, RelayExecutionState::Idle, "{outcome}");
        assert!(state.harness_turn.is_none());
        assert!(state.foreground_tool_started_at_ms.is_none());
        assert_eq!(state.background_commands.len(), 2);
        assert!(
            !state.is_quiet(),
            "background agents must prevent worker replacement"
        );

        // A replacement level clears missing tasks even without a completion
        // bookend, and keeps the clock of a task that remains live.
        relay
            .claude_background_tasks
            .get_mut("design")
            .unwrap()
            .started_at_ms = 123;
        relay
            .claude_background_tasks_changed(vec![claude_task("design", "Design cleanup")])
            .unwrap();
        assert_eq!(
            relay.operational_state().background_commands,
            vec![BackgroundCommand {
                id: "claude:design".into(),
                started_at_ms: 123,
                command: "Design cleanup".into(),
                can_stop: false,
            }]
        );
        relay.claude_background_tasks_changed(Vec::new()).unwrap();
        assert!(relay.operational_state().is_quiet());
    }
}

#[test]
fn background_task_stop_targets_are_live_capability_checked_and_namespaced() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay
        .agent_terminal_started(ActiveAgentTerminal {
            terminal_id: "shared-id".into(),
            command: "cargo test".into(),
            started_at_ms: 10,
        })
        .unwrap();
    relay
        .claude_background_tasks_changed(vec![claude_task("shared-id", "Review tests")])
        .unwrap();

    assert_eq!(
        relay
            .background_task_stop_target("terminal:shared-id")
            .unwrap(),
        BackgroundTaskStopTarget::HostedTerminal {
            terminal_id: "shared-id".into(),
        }
    );
    assert!(
        relay
            .background_task_stop_target("claude:shared-id")
            .is_err(),
        "Claude must not be stoppable until AIR grants the affordance"
    );

    relay
        .claude_async_task_control_changed("shared-id".into(), true)
        .unwrap();
    assert_eq!(
        relay
            .background_task_stop_target("claude:shared-id")
            .unwrap(),
        BackgroundTaskStopTarget::ClaudeAsyncTask {
            task_id: "shared-id".into(),
        }
    );
    assert!(
        relay
            .operational_state()
            .background_commands
            .iter()
            .any(|command| command.id == "claude:shared-id" && command.can_stop)
    );

    relay
        .claude_async_task_control_changed("shared-id".into(), false)
        .unwrap();
    assert!(
        relay
            .background_task_stop_target("claude:shared-id")
            .is_err()
    );
    relay.agent_terminal_closed("shared-id").unwrap();
    assert!(
        relay
            .background_task_stop_target("terminal:shared-id")
            .is_err(),
        "a stale UI id must never resolve after its task exits"
    );
}

/// A plain agent chunk carrying only `text`, as the Claude adapter sends
/// when it acknowledges a task stop.
fn claude_stop_acknowledgement(name: &str) -> String {
    format!("{CLAUDE_STOP_ACKNOWLEDGEMENT_PREFIX}{name}.")
}

fn agent_text_chunk(text: &str) -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ContentChunk, TextContent};
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

/// A Claude relay with one stoppable task `sleeper` named "Sleep 600".
fn claude_relay_with_stoppable_task(root: &std::path::Path) -> DurableRelay {
    let mut relay = claude_relay(root);
    relay
        .claude_background_tasks_changed(vec![claude_task("sleeper", "Sleep 600")])
        .unwrap();
    relay
        .claude_async_task_control_changed("sleeper".into(), true)
        .unwrap();
    relay
}

fn assert_chunk_opened_a_harness_turn(relay: &DurableRelay) {
    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Running);
    assert!(state.harness_turn.is_some(), "the chunk must open a turn");
    assert!(matches!(
        observations(relay).as_slice(),
        [
            RelayObservation::HarnessTurnStarted { .. },
            RelayObservation::SessionUpdate { .. }
        ]
    ));
}

#[test]
fn a_requested_stop_acknowledgement_does_not_open_a_harness_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay_with_stoppable_task(temp.path());
    assert_eq!(
        relay.background_task_stop_target("claude:sleeper").unwrap(),
        BackgroundTaskStopTarget::ClaudeAsyncTask {
            task_id: "sleeper".into(),
        }
    );

    relay
        .record_session_update(agent_text_chunk(&claude_stop_acknowledgement("Sleep 600")))
        .unwrap();

    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert!(
        state.harness_turn.is_none(),
        "no model turn follows a user-requested stop, so nothing would settle one"
    );
    assert!(
        matches!(
            observations(&relay).as_slice(),
            [RelayObservation::SessionUpdate { update }]
                if agent_chunk_text(update) == Some(&claude_stop_acknowledgement("Sleep 600"))
        ),
        "the acknowledgement still enters the transcript"
    );

    // Genuine work after the acknowledgement opens a turn as before.
    relay.record_session_update(tool_call_update()).unwrap();
    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Running);
    assert!(state.harness_turn.is_some());
}

#[test]
fn a_stop_acknowledgement_matches_whatever_name_the_adapter_now_uses() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay_with_stoppable_task(temp.path());
    relay.background_task_stop_target("claude:sleeper").unwrap();

    // The level said "Sleep 600"; a later task_started renamed it to the
    // command text, which is what the adapter quotes.
    relay
        .record_session_update(agent_text_chunk(&claude_stop_acknowledgement("sleep 900")))
        .unwrap();

    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert!(state.harness_turn.is_none());
    assert!(relay.claude_pending_stops.is_empty());
}

#[test]
fn an_unrequested_stop_text_still_opens_a_harness_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay_with_stoppable_task(temp.path());

    relay
        .record_session_update(agent_text_chunk(&claude_stop_acknowledgement("Sleep 600")))
        .unwrap();

    assert_chunk_opened_a_harness_turn(&relay);
}

#[test]
fn a_failed_stop_clears_its_acknowledgement() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay_with_stoppable_task(temp.path());
    relay.background_task_stop_target("claude:sleeper").unwrap();
    relay.claude_stop_not_sent("sleeper");

    relay
        .record_session_update(agent_text_chunk(&claude_stop_acknowledgement("Sleep 600")))
        .unwrap();

    assert_chunk_opened_a_harness_turn(&relay);
}

#[test]
fn stop_acknowledgements_clear_on_restart() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay_with_stoppable_task(temp.path());
    relay.background_task_stop_target("claude:sleeper").unwrap();
    relay
        .record_observation(RelayObservation::SessionRestarted)
        .unwrap();
    assert!(relay.claude_pending_stops.is_empty());

    relay
        .record_session_update(agent_text_chunk(&claude_stop_acknowledgement("Sleep 600")))
        .unwrap();

    let state = relay.operational_state();
    assert!(state.harness_turn.is_some(), "the chunk must open a turn");
    assert!(matches!(
        observations(&relay).as_slice(),
        [
            RelayObservation::SessionRestarted,
            RelayObservation::HarnessTurnStarted { .. },
            RelayObservation::SessionUpdate { .. }
        ]
    ));
}

#[test]
fn claude_background_levels_do_not_open_turns_or_enter_the_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    let ordinal = relay.snapshot.latest_ordinal;
    relay
        .claude_background_tasks_changed(vec![claude_task("workflow", "Design reviews")])
        .unwrap();
    assert_eq!(relay.snapshot.latest_ordinal, ordinal);
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Idle
    );
    assert!(relay.operational_state().harness_turn.is_none());

    // An autonomous follow-up keeps its foreground state while tasks live,
    // and settling that turn returns to background work.
    relay.record_session_update(tool_call_update()).unwrap();
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Running
    );
    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Idle
    );
    assert_eq!(relay.operational_state().background_commands.len(), 1);

    relay
        .agent_terminal_started(ActiveAgentTerminal {
            terminal_id: "shell".into(),
            command: "sleep 600".into(),
            started_at_ms: 1,
        })
        .unwrap();
    assert_eq!(relay.operational_state().background_commands.len(), 2);
    relay.claude_background_tasks_changed(Vec::new()).unwrap();
    assert_eq!(relay.operational_state().background_commands.len(), 1);
    relay.agent_terminal_closed("shell").unwrap();
    assert!(relay.operational_state().is_quiet());
}

#[test]
fn claude_background_tasks_are_process_local_and_clear_on_teardown() {
    for observation in [
        RelayObservation::SessionRestarted,
        RelayObservation::Closing,
        RelayObservation::Closed,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(temp.path());
        relay
            .claude_background_tasks_changed(vec![claude_task("design", "Design review")])
            .unwrap();
        relay.record_observation(observation).unwrap();
        assert!(relay.operational_state().background_commands.is_empty());
    }
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay
        .claude_background_tasks_changed(vec![claude_task("design", "Design review")])
        .unwrap();
    relay.clear_agent_terminals().unwrap();
    assert!(relay.operational_state().background_commands.is_empty());
    relay
        .claude_background_tasks_changed(vec![claude_task("design", "Design review")])
        .unwrap();
    drop(relay);
    let relay = claude_relay(temp.path());
    assert!(relay.operational_state().background_commands.is_empty());
}

#[test]
fn a_codex_exec_card_without_an_exit_code_is_background_work_until_one_arrives() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);

    let mut started = agent_client_protocol::schema::v1::ToolCall::new("call-1", "shell");
    started.kind = agent_client_protocol::schema::v1::ToolKind::Execute;
    started.status = agent_client_protocol::schema::v1::ToolCallStatus::InProgress;
    started.raw_input = Some(serde_json::json!({
        "command": ["bash", "-lc", "sleep 600"],
    }));
    relay
        .record_session_update(SessionUpdate::ToolCall(started))
        .unwrap();
    assert!(
        relay.operational_state().background_commands.is_empty(),
        "an execute card is not background work before it reports a detached result"
    );

    relay
        .record_session_update(exec_card_update("call-1", None))
        .unwrap();

    let commands = relay.operational_state().background_commands;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, "codex:call-1");
    assert_eq!(commands[0].command, "bash -lc sleep 600");
    assert!(commands[0].started_at_ms > 0);
    assert!(!commands[0].can_stop);
    assert!(relay.background_task_stop_target("codex:call-1").is_err());

    // A card that does report an exit code says the process is done, even
    // when it is the first card for that call.
    relay
        .record_session_update(exec_card("call-2", &["ls"], Some(0)))
        .unwrap();
    assert_eq!(
        relay.operational_state().background_commands.len(),
        1,
        "a finished command is not background work"
    );

    // Codex polls the process it left running; the poll carries the exit.
    relay
        .record_session_update(exec_card_update("call-1", Some(0)))
        .unwrap();
    assert!(relay.operational_state().background_commands.is_empty());
}

#[test]
fn completed_codex_mcp_execute_calls_do_not_leave_background_work() {
    use agent_client_protocol::schema::v1::{
        ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };

    for partial in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        submit_relay(&mut relay, "memory-prompt", prompt("remember the result"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);

        let mut call = ToolCall::new("memory", "mcp.mj-memory.write");
        call.kind = ToolKind::Execute;
        call.raw_input = Some(serde_json::json!({
            "server": "mj-memory", "tool": "write",
            "arguments": {"path": "/MEMORY.md", "content": "done"}
        }));
        let output = serde_json::json!({
            "result": {"content": [{"type": "text", "text": "saved"}]},
            "error": null
        });
        if partial {
            call.status = ToolCallStatus::InProgress;
            relay
                .record_session_update(SessionUpdate::ToolCall(call))
                .unwrap();
            relay
                .record_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    "memory",
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::Completed)
                        .raw_output(output),
                )))
                .unwrap();
        } else {
            call.status = ToolCallStatus::Completed;
            call.raw_output = Some(output);
            relay
                .record_session_update(SessionUpdate::ToolCall(call))
                .unwrap();
        }
        assert!(relay.operational_state().background_commands.is_empty());
        relay
            .record_command_completed(
                "memory-prompt",
                RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
        assert!(relay.operational_state().is_quiet());
    }
}

#[test]
fn a_codex_non_execute_partial_result_is_not_background_work() {
    use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};

    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);

    let mut guardian =
        agent_client_protocol::schema::v1::ToolCall::new("guardian-assessment", "Guardian Review");
    guardian.kind = agent_client_protocol::schema::v1::ToolKind::Think;
    guardian.status = agent_client_protocol::schema::v1::ToolCallStatus::InProgress;
    relay
        .record_session_update(SessionUpdate::ToolCall(guardian))
        .unwrap();
    relay
        .record_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "guardian-assessment",
            ToolCallUpdateFields::new()
                .status(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                .raw_output(serde_json::json!({"review": {"status": "approved"}})),
        )))
        .unwrap();

    assert!(
        relay.operational_state().background_commands.is_empty(),
        "a partial result inherits the original non-execute kind"
    );
}

#[test]
fn prompt_boundaries_clear_stale_tools_but_preserve_detached_commands() {
    for outcome in ["completed", "rejected", "interrupted"] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
        submit_relay(&mut relay, "boundary-prompt", prompt("run work"));
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        relay.record_session_update(tool_call_update()).unwrap();
        relay
            .record_session_update(exec_card("detached", &["sleep", "600"], None))
            .unwrap();
        assert!(
            relay
                .operational_state()
                .foreground_tool_started_at_ms
                .is_some()
        );

        match outcome {
            "completed" => relay.record_command_completed(
                "boundary-prompt",
                RelayCommandOutcome::Prompt {
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            ),
            "rejected" => relay.record_command_rejected("boundary-prompt", "adapter failed"),
            _ => relay.record_command_interrupted("boundary-prompt", "cancelled"),
        }
        .unwrap();

        let state = relay.operational_state();
        assert_eq!(state.foreground_tool_started_at_ms, None, "{outcome}");
        assert_eq!(state.background_commands.len(), 1, "{outcome}");
        relay
            .record_session_update(exec_card_update("detached", Some(0)))
            .unwrap();
        assert!(
            relay.operational_state().background_commands.is_empty(),
            "{outcome}"
        );
    }
}

#[test]
fn an_unsettled_tool_call_is_foreground_work_even_without_a_turn_marker() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();

    relay.record_session_update(tool_call_update()).unwrap();
    let state = relay.operational_state();
    assert!(
        state.foreground_tool_started_at_ms.is_some(),
        "a pending tool is positive foreground-work evidence"
    );
    assert!(
        !state.is_quiet(),
        "a pending foreground tool blocks replacement"
    );

    relay
        .record_session_update(exec_card("call-1", &["true"], Some(0)))
        .unwrap();
    let state = relay.operational_state();
    assert_eq!(
        state.foreground_tool_started_at_ms, None,
        "a settled tool no longer overrides background work"
    );
    assert!(
        state.is_quiet(),
        "a settled foreground tool permits replacement"
    );
}

#[test]
fn completed_subagent_tool_update_keeps_a_parent_prompt_busy() {
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};

    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    submit_relay(&mut relay, "parent-prompt", prompt("keep working"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "parent-prompt"
    );

    let mut subagent = ToolCall::new("m4_csharp", "Start subagent m4_csharp");
    subagent.status = ToolCallStatus::Completed;
    relay
        .record_session_update(SessionUpdate::ToolCall(subagent))
        .unwrap();

    let state = relay.operational_state();
    assert_eq!(
        state
            .active_prompt
            .as_ref()
            .map(|prompt| prompt.command_id.as_str()),
        Some("parent-prompt")
    );
    assert!(state.harness_turn.is_none());
    assert!(!state.is_quiet(), "the parent prompt is still in flight");

    relay
        .record_command_completed(
            "parent-prompt",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    let state = relay.operational_state();
    assert!(state.active_prompt.is_none());
    assert!(state.harness_turn.is_none());
    assert!(
        state.is_quiet(),
        "prompt completion releases the busy guard"
    );
}

#[test]
fn a_restart_forgets_the_commands_the_previous_harness_left_running() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
    relay
        .record_session_update(exec_card("call-1", &["sleep", "600"], None))
        .unwrap();
    assert_eq!(relay.operational_state().background_commands.len(), 1);

    relay
        .record_observation(RelayObservation::SessionRestarted)
        .unwrap();

    assert!(
        relay.operational_state().background_commands.is_empty(),
        "the harness that owned those processes is gone"
    );
}

#[test]
fn harness_turns_are_off_for_other_harnesses() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();

    relay.record_session_update(tool_call_update()).unwrap();
    relay.record_session_update(origin_marker("human")).unwrap();
    relay.claude_turn_result(&cycle_result("human")).unwrap();

    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert!(state.harness_turn.is_none());
    assert!(state.last_harness_turn_started_ordinal.is_none());
    assert!(
        observations(&relay)
            .iter()
            .all(|observation| matches!(observation, RelayObservation::SessionUpdate { .. })),
        "only the updates themselves are journaled"
    );
}

#[test]
fn an_in_flight_prompt_blocks_checkpoint_barrier_admission() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "stuck-prompt", prompt("keep running"));
    let prompt = relay.claim_pending_commands(true).unwrap();
    assert_eq!(prompt.len(), 1);
    assert_eq!(prompt[0].command_id, "stuck-prompt");

    submit_relay(
        &mut relay,
        "barrier-command",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    assert!(
        relay.claim_pending_commands(true).unwrap().is_empty(),
        "a live ACP turn must keep the checkpoint barrier queued"
    );

    relay
        .record_command_interrupted("stuck-prompt", "worker restarted")
        .unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "barrier-command");
}

#[test]
fn cancel_dispatches_while_the_prompt_it_targets_is_in_flight() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "cancelled-prompt", prompt("keep running"));
    let claimed_prompt = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed_prompt.len(), 1);
    assert_eq!(claimed_prompt[0].command_id, "cancelled-prompt");

    submit_relay(&mut relay, "queued-correction", prompt("change direction"));
    submit_relay(&mut relay, "cancel-command", RelayCommand::Cancel);
    let cancel = relay.claim_pending_commands(true).unwrap();
    assert_eq!(cancel.len(), 1);
    assert_eq!(cancel[0].command_id, "cancel-command");
    assert!(matches!(cancel[0].command, RelayCommand::Cancel));
    let steering = cancel[0]
        .steering_prompt
        .as_ref()
        .expect("cancel carries the queued prompt head");
    assert_eq!(steering.queued_command_id, "queued-correction");
    assert_eq!(
        steering.prompt,
        vec![ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new("change direction")
        )]
    );

    relay
        .record_command_completed(
            "cancel-command",
            RelayCommandOutcome::Steered {
                queued_command_id: "queued-correction".into(),
            },
        )
        .unwrap();
    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Running);
    assert_eq!(state.active_prompt.unwrap().command_id, "cancelled-prompt");
    assert!(state.queued_prompts.is_empty());

    let mut session = mj_core::state::MaterializedSession::empty(SESSION);
    for event in relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap() {
        let projected = mj_transcript::projection::project_relay_event(&session, &event).unwrap();
        mj_transcript::projection::apply_committed_projection_event(
            &mut session,
            &event,
            projected.mutation,
        )
        .unwrap();
    }
    assert!(matches!(
        session.execution,
        mj_core::state::MaterializedExecutionState::Running { .. }
    ));
    assert!(session.queued_prompts.is_empty());
    assert_eq!(
        session
            .transcript
            .iter()
            .filter(|item| matches!(item.body, mj_core::state::TranscriptBody::User { .. }))
            .count(),
        2
    );
}

#[test]
fn cancel_turn_bypasses_a_pending_checkpoint_without_steering_the_queue() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "active-prompt", prompt("keep running"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "active-prompt"
    );
    submit_relay(
        &mut relay,
        "pending-checkpoint",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    submit_relay(&mut relay, "queued-prompt", prompt("leave queued"));
    submit_relay(&mut relay, "cancel-turn", RelayCommand::CancelTurn);

    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "cancel-turn");
    assert!(matches!(claimed[0].command, RelayCommand::CancelTurn));
    assert!(claimed[0].steering_prompt.is_none());
    assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);

    relay
        .record_command_completed("cancel-turn", RelayCommandOutcome::Cancelled)
        .unwrap();
    let state = relay.operational_state();
    assert_eq!(state.active_prompt.unwrap().command_id, "active-prompt");
    assert!(state.harness_turn.is_none());
    assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);
    assert!(state.checkpoint_barrier.is_none());
}

#[test]
fn cancel_turn_bypasses_a_pending_checkpoint_for_an_autonomous_turn() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = claude_relay(temp.path());
    relay.record_session_update(tool_call_update()).unwrap();
    assert!(relay.operational_state().harness_turn.is_some());
    submit_relay(
        &mut relay,
        "pending-checkpoint",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    submit_relay(&mut relay, "queued-prompt", prompt("leave queued"));
    submit_relay(&mut relay, "cancel-turn", RelayCommand::CancelTurn);

    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "cancel-turn");
    assert!(claimed[0].steering_prompt.is_none());
    assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);

    relay
        .record_command_completed("cancel-turn", RelayCommandOutcome::Cancelled)
        .unwrap();
    let state = relay.operational_state();
    assert!(state.harness_turn.is_some());
    assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);

    relay
        .claude_turn_result(&cycle_result("task-notification"))
        .unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "pending-checkpoint");
    assert_eq!(queued_command_ids(&relay), vec!["queued-prompt"]);
}

#[test]
fn cancel_turn_never_bypasses_an_admitted_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "admitted-checkpoint",
        RelayCommand::BeginCheckpoint { reason: None },
    );
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "admitted-checkpoint"
    );
    relay
        .record_checkpoint_ready("admitted-checkpoint")
        .unwrap();
    let before = relay.operational_state();
    let error = relay
        .submit_command("cancel-turn", RelayCommand::CancelTurn)
        .unwrap()
        .expect_err("late cancellation must leave the checkpoint cursor intact");
    assert_eq!(error.code, RelayErrorCode::InvalidState);
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    assert_eq!(relay.operational_state(), before);
    assert!(!relay.snapshot.dispatches.contains_key("cancel-turn"));
}

#[test]
fn cancel_turn_is_harmless_when_the_relay_is_idle() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    submit_relay(&mut relay, "cancel-idle", RelayCommand::CancelTurn);
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert!(claimed[0].steering_prompt.is_none());
    relay
        .record_command_completed("cancel-idle", RelayCommandOutcome::Cancelled)
        .unwrap();

    let state = relay.operational_state();
    assert_eq!(state.execution, RelayExecutionState::Idle);
    assert!(state.active_prompt.is_none());
    assert!(state.harness_turn.is_none());
    assert!(state.is_quiet());
}

#[test]
fn config_accepted_during_a_prompt_waits_while_cancel_bypasses_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "active-prompt", prompt("keep running"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "active-prompt"
    );

    submit_relay(
        &mut relay,
        "config-after-prompt",
        set_config("model", "later"),
    );
    submit_relay(&mut relay, "cancel-after-config", RelayCommand::Cancel);

    let cancel = relay.claim_pending_commands(true).unwrap();
    assert_eq!(cancel.len(), 1);
    assert_eq!(cancel[0].command_id, "cancel-after-config");
    assert!(matches!(cancel[0].command, RelayCommand::Cancel));
    assert!(cancel[0].steering_prompt.is_none());
    relay
        .record_command_completed("cancel-after-config", RelayCommandOutcome::Cancelled)
        .unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    relay
        .record_command_completed(
            "active-prompt",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "cancelled".into(),
                usage: None,
            },
        )
        .unwrap();
    let config = relay.claim_pending_commands(true).unwrap();
    assert_eq!(config.len(), 1);
    assert_eq!(config[0].command_id, "config-after-prompt");
}

#[test]
fn config_accepted_before_a_prompt_keeps_acceptance_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "config-first", set_config("model", "first"));
    submit_relay(&mut relay, "prompt-second", prompt("then run"));

    // Queue entries run one at a time, so the prompt waits for the
    // configuration change accepted before it.
    let config = relay.claim_pending_commands(true).unwrap();
    assert_eq!(config.len(), 1);
    assert_eq!(config[0].command_id, "config-first");
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    relay
        .record_command_completed("config-first", RelayCommandOutcome::Configured)
        .unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "prompt-second");
}

#[test]
fn config_queued_behind_a_prompt_applies_in_queue_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "prompt-one", prompt("one"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "prompt-one"
    );

    submit_relay(&mut relay, "prompt-two", prompt("two"));
    submit_relay(&mut relay, "config-third", set_config("model", "sonnet"));
    submit_relay(&mut relay, "prompt-four", prompt("four"));
    assert_eq!(
        queued_command_ids(&relay),
        ["prompt-two", "config-third", "prompt-four"]
    );
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    finish_prompt(&mut relay, "prompt-one");
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "prompt-two"
    );
    finish_prompt(&mut relay, "prompt-two");

    let config = relay.claim_pending_commands(true).unwrap();
    assert_eq!(config.len(), 1);
    assert_eq!(config[0].command_id, "config-third");
    assert!(matches!(config[0].command, RelayCommand::SetConfig { .. }));
    // A configuration change applies between turns, so the relay stays
    // idle and the prompt behind it waits for the change to finish.
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Idle
    );
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    relay
        .record_command_completed("config-third", RelayCommandOutcome::Configured)
        .unwrap();
    assert_eq!(
        relay
            .operational_state()
            .config
            .get("model")
            .map(String::as_str),
        Some("sonnet")
    );
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "prompt-four"
    );
}

#[test]
fn removing_a_queued_config_stops_it_from_dispatching() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "active-prompt", prompt("running"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "active-prompt"
    );
    submit_relay(&mut relay, "config-queued", set_config("effort", "high"));
    submit_relay(
        &mut relay,
        "remove-config",
        RelayCommand::RemoveQueuedPrompt {
            queued_command_id: "config-queued".into(),
        },
    );

    assert!(queued_command_ids(&relay).is_empty());
    assert_eq!(
        relay.snapshot.dispatches["config-queued"].state,
        RelayDispatchState::Rejected
    );
    assert!(
        relay.snapshot.handled_commands["config-queued"]
            .terminal_ordinal
            .is_some()
    );

    finish_prompt(&mut relay, "active-prompt");
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
    assert!(relay.operational_state().config.is_empty());
}

#[test]
fn clearing_the_queue_drops_queued_configuration_changes() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "active-prompt", prompt("running"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "active-prompt"
    );
    submit_relay(&mut relay, "queued-prompt", prompt("later"));
    submit_relay(&mut relay, "queued-config", set_config("model", "later"));

    submit_relay(&mut relay, "clear-queue", RelayCommand::ClearQueuedPrompts);
    assert!(queued_command_ids(&relay).is_empty());

    finish_prompt(&mut relay, "active-prompt");
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
}

#[test]
fn an_incomplete_configuration_change_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let response = relay.handle(relay_request(
        "reject-empty-config",
        RelayRequest::Submit {
            command_id: "empty-config".into(),
            command: set_config("model", "  "),
        },
    ));
    assert!(matches!(
        response.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidRequest,
                ..
            }
        }
    ));
    assert!(queued_command_ids(&relay).is_empty());
}

#[test]
fn close_requires_exact_checkpoint_cut_and_survives_controller_disconnect() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let stale_cut = ready_checkpoint(&mut relay, "stale-close-barrier");
    relay
        .record_observation(RelayObservation::Warning {
            message: "post-cut drift".into(),
        })
        .unwrap();
    let rejected = relay.handle(relay_request(
        "reject-stale-close",
        RelayRequest::Submit {
            command_id: "stale-close-command".into(),
            command: RelayCommand::Close {
                barrier_command_id: "stale-close-barrier".into(),
                expected: stale_cut,
            },
        },
    ));
    assert!(matches!(
        rejected.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));
    relay
        .cancel_checkpoint_barrier_on_disconnect("stale-close-barrier")
        .unwrap();

    let exact_cut = ready_checkpoint(&mut relay, "exact-close-barrier");
    let accepted = submit_relay(
        &mut relay,
        "exact-close-command",
        RelayCommand::Close {
            barrier_command_id: "exact-close-barrier".into(),
            expected: exact_cut.clone(),
        },
    );
    assert!(accepted > exact_cut.ordinal);
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Closing
    );
    let later = relay.handle(relay_request(
        "post-close-command",
        RelayRequest::Submit {
            command_id: "post-close-prompt".into(),
            command: prompt("must not run"),
        },
    ));
    assert!(matches!(
        later.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));

    assert!(
        relay
            .cancel_checkpoint_barrier_on_disconnect("exact-close-barrier")
            .unwrap()
            .is_some()
    );
    let close = relay.claim_pending_commands(true).unwrap();
    assert_eq!(close.len(), 1);
    assert_eq!(close[0].command_id, "exact-close-command");
    assert!(matches!(close[0].command, RelayCommand::Close { .. }));
}

#[test]
fn exact_close_allows_checkpoint_completion_before_close_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let expected = ready_checkpoint(&mut relay, "normal-close-barrier");
    submit_relay(
        &mut relay,
        "normal-close-command",
        RelayCommand::Close {
            barrier_command_id: "normal-close-barrier".into(),
            expected,
        },
    );
    submit_relay(
        &mut relay,
        "normal-close-complete",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "normal-close-barrier".into(),
        },
    );

    let close = relay.claim_pending_commands(true).unwrap();
    assert_eq!(close.len(), 1);
    assert_eq!(close[0].command_id, "normal-close-command");
    assert!(matches!(close[0].command, RelayCommand::Close { .. }));
}

#[test]
fn credential_requests_cannot_enter_durable_relay_state() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();

    for (request_id, request) in [
        ("credential-state", RelayRequest::CredentialState),
        ("read-credentials", RelayRequest::ReadCredentials),
        (
            "install-credentials",
            RelayRequest::InstallCredentials {
                data: "e30=".into(),
            },
        ),
    ] {
        let response = relay.handle(relay_request(request_id, request));
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    retryable: false,
                    ..
                }
            }
        ));
    }

    assert_eq!(relay.latest_ordinal(), 0);
    assert!(
        relay
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .is_empty()
    );
    let persisted = fs::read_to_string(temp.path().join(RELAY_STATE_FILE)).unwrap();
    assert!(!persisted.contains("e30="));
    assert!(relay.snapshot.handled_commands.is_empty());
}
#[test]
fn goal_controls_bypass_work_and_checkpoints_without_consuming_prompts() {
    use mj_core::goal::GoalControlAction;
    for autonomous in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = if autonomous {
            claude_relay(temp.path())
        } else {
            DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap()
        };
        if autonomous {
            relay.record_session_update(tool_call_update()).unwrap();
        } else {
            submit_relay(&mut relay, "active-work", prompt("keep running"));
            relay.claim_pending_commands(true).unwrap();
        }
        submit_relay(
            &mut relay,
            "checkpoint",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        submit_relay(&mut relay, "queued-work", prompt("leave queued"));
        for action in GoalControlAction::ALL {
            submit_relay(
                &mut relay,
                &format!("goal-{}", action.as_str()),
                RelayCommand::GoalControl { action },
            );
            let claimed = relay.claim_pending_commands(true).unwrap();
            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].command_id, format!("goal-{}", action.as_str()));
            assert!(claimed[0].steering_prompt.is_none());
            // Leave resume outstanding while sending subsequent clear.
            if action != GoalControlAction::Resume {
                relay
                    .record_command_completed(
                        &format!("goal-{}", action.as_str()),
                        RelayCommandOutcome::GoalControlled,
                    )
                    .unwrap();
            }
        }
        assert_eq!(queued_command_ids(&relay), vec!["queued-work"]);
        assert!(relay.operational_state().checkpoint_barrier.is_none());
        assert_eq!(relay.operational_state().harness_turn.is_some(), autonomous);
        assert_eq!(
            relay.operational_state().active_prompt.is_some(),
            !autonomous
        );
        let events = relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
        let mut projection = mj_core::state::MaterializedSession::empty(SESSION);
        for event in &events {
            let projected =
                mj_transcript::projection::project_relay_event(&projection, event).unwrap();
            mj_transcript::projection::apply_committed_projection_event(
                &mut projection,
                event,
                projected.mutation,
            )
            .unwrap();
        }
        drop(relay);
        let relay = DurableRelay::open_for_checkpoint(temp.path(), SESSION, "1.0.0").unwrap();
        assert_eq!(queued_command_ids(&relay), vec!["queued-work"]);
        let replay = relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
        assert!(replay.starts_with(&events));
        assert!(matches!(
            relay.snapshot.dispatches["goal-resume"].state,
            RelayDispatchState::Interrupted
        ));
    }
}

#[test]
fn continuation_expectation_is_process_local_and_clears_when_a_harness_turn_opens() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let generation = relay.turn_context().generation();
    relay
        .expect_continuation(1_234, "waiting for work".into(), generation)
        .unwrap();
    assert!(matches!(
        relay.operational_state().activity_state(),
        mj_core::activity::ActivityState::Expecting { since_ms: 1_234 }
    ));
    assert_eq!(relay.activity_facts(), relay.operational_state().facts());
    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(reopened.activity_facts().expected_continuation, None);
    drop(reopened);
    relay
        .record_observation(RelayObservation::HarnessTurnStarted {
            started_at_ms: 2_000,
        })
        .unwrap();
    assert_eq!(relay.activity_facts().expected_continuation, None);
    relay
        .record_observation(RelayObservation::HarnessTurnSettled {
            origin: None,
            prompt_in_flight: false,
        })
        .unwrap();
    assert_eq!(relay.activity_facts().expected_continuation, None);
}

#[test]
fn new_prompt_invalidates_a_completed_turn_verdict_and_resets_its_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let old_generation = relay.turn_context().generation();
    relay
        .expect_continuation(1_234, String::new(), old_generation)
        .unwrap();
    submit_relay(
        &mut relay,
        "new-prompt",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("new instructions")],
        },
    );
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    assert_eq!(relay.activity_facts().expected_continuation, None);
    let evidence = relay.turn_context().evidence(
        mj_core::config::HarnessKind::Claude,
        mj_core::activity::verdict::TurnPhase::Running,
        &relay.activity_facts(),
        2_000,
    );
    assert_eq!(evidence.user_prompt_tail, "new instructions");
    relay
        .record_command_completed(
            "new-prompt",
            RelayCommandOutcome::Prompt {
                stop_reason: "EndTurn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    relay
        .expect_continuation(3_000, String::new(), old_generation)
        .unwrap();
    assert_eq!(relay.activity_facts().expected_continuation, None);
}

#[test]
fn finished_turn_queues_only_one_replied_classification() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
    submit_relay(
        &mut relay,
        "prompt-1",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("instructions")],
        },
    );
    relay.claim_pending_commands(true).unwrap();
    assert!(relay.pending_replied_verdict().is_none());
    relay
        .record_command_completed(
            "prompt-1",
            RelayCommandOutcome::Prompt {
                stop_reason: "EndTurn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    let (_, evidence, _) = relay.pending_replied_verdict().unwrap();
    assert_eq!(
        evidence.phase,
        mj_core::activity::verdict::TurnPhase::Replied
    );
    assert_eq!(evidence.user_prompt_tail, "instructions");
    assert!(relay.pending_replied_verdict().is_none());
}

#[test]
fn configuration_notice_is_recorded_only_after_confirmed_completion() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(
        &mut relay,
        "change-model",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "new-model".into(),
        },
    );
    relay.claim_pending_commands(true).unwrap();
    assert!(
        !relay
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .iter()
            .any(|event| matches!(event.observation, RelayObservation::Notice { .. }))
    );
    relay
        .record_command_completed("change-model", RelayCommandOutcome::Configured)
        .unwrap();
    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let events = reopened
        .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
        .unwrap();
    assert_eq!(events.iter().filter(|event|
        matches!(&event.observation, RelayObservation::Notice { message } if message == "model set to new-model")
    ).count(), 1);
}

#[test]
fn compact_preserves_large_pending_context_for_the_next_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    let context = "shell output\n".repeat(8192);
    relay.install_prompt_context(context.clone()).unwrap();
    submit_relay(
        &mut relay,
        "compact-command",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("/compact")],
        },
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert!(claimed[0].hidden_prompt_context.is_none());
    relay
        .record_command_completed(
            "compact-command",
            RelayCommandOutcome::Prompt {
                stop_reason: "EndTurn".into(),
                usage: None,
                diagnostic: None,
            },
        )
        .unwrap();
    submit_relay(
        &mut relay,
        "followup-command",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("continue")],
        },
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(
        claimed[0].hidden_prompt_context.as_deref(),
        Some(context.as_str())
    );
}

fn clearable_relay(root: &Path) -> DurableRelay {
    let mut relay = DurableRelay::open(root, SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Codex);
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "original".into(),
            resumed: false,
            native_continuity_lost: false,
        })
        .unwrap();
    relay.mark_native_session_used().unwrap();
    relay
}

#[test]
fn clear_changes_only_native_context_and_is_idempotent_across_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = clearable_relay(temp.path());
    relay
        .install_prompt_context("old conversation handoff".into())
        .unwrap();
    relay
        .snapshot
        .config
        .insert("model".into(), "chosen-model".into());
    let clear = RelayCommand::Prompt {
        prompt: vec![ContentBlock::from("/clear")],
    };
    assert!(
        relay
            .submit_command("clear-request", clear.clone())
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Running
    );
    assert!(
        relay
            .submit_command(
                "later-prompt",
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::from("must wait")],
                }
            )
            .unwrap()
            .is_err()
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command, RelayCommand::ClearContext);
    assert_eq!(
        relay.operational_state().native_session_id.as_deref(),
        Some("original")
    );
    relay
        .record_command_completed(
            "clear-request",
            RelayCommandOutcome::ContextCleared {
                native_session_id: "replacement".into(),
                memory: None,
            },
        )
        .unwrap();
    let ordinal = relay.operational_state().latest_ordinal;
    assert!(!relay.native_session_may_have_history());
    assert!(relay.snapshot.pending_prompt_context.is_none());
    assert_eq!(
        relay.snapshot.config.get("model").map(String::as_str),
        Some("chosen-model")
    );
    drop(relay);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Codex);
    assert!(
        relay
            .submit_command("clear-request", clear)
            .unwrap()
            .is_ok()
    );
    assert_eq!(relay.operational_state().latest_ordinal, ordinal);
    assert_eq!(
        relay.operational_state().native_session_id.as_deref(),
        Some("replacement")
    );
    assert!(!relay.native_session_may_have_history());
}

#[test]
fn interrupted_clear_keeps_the_original_native_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = clearable_relay(temp.path());
    assert!(
        relay
            .submit_command("clear-request", RelayCommand::ClearContext)
            .unwrap()
            .is_ok()
    );
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    drop(relay);
    let relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    assert_eq!(
        relay.operational_state().native_session_id.as_deref(),
        Some("original")
    );
    assert!(relay.native_session_may_have_history());
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Idle
    );
}

#[test]
fn clear_refuses_pending_work_and_invalid_input_without_changing_identity() {
    for input in ["/clear extra", "/compact ignored-instructions"] {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = clearable_relay(temp.path());
        assert!(
            relay
                .submit_command(
                    "invalid-command",
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::from(input)],
                    }
                )
                .unwrap()
                .is_err()
        );
        assert_eq!(
            relay.operational_state().native_session_id.as_deref(),
            Some("original")
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let mut relay = clearable_relay(temp.path());
    submit_relay(
        &mut relay,
        "pending-work",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("do work")],
        },
    );
    assert!(
        relay
            .submit_command("clear-request", RelayCommand::ClearContext)
            .unwrap()
            .is_err()
    );
    assert!(relay.snapshot.dispatches.contains_key("pending-work"));
}

#[test]
fn jev_messages_survive_reopen_without_tool_details() {
    use agent_client_protocol::schema::v1::{
        ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };
    use mj_core::activity::{ActivityFacts, verdict::TurnPhase};
    use mj_core::config::HarnessKind;
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    submit_relay(&mut relay, "prompt-summary", prompt("Summarize this work"));
    relay.claim_pending_commands(true).unwrap();
    relay.record_session_update(serde_json::from_value(serde_json::json!({
        "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Preserved answer"}
    })).unwrap()).unwrap();
    for n in 0..9 {
        relay
            .record_session_update(SessionUpdate::ToolCall(
                ToolCall::new(format!("call-{n}"), "Execute")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Completed)
                    .raw_input(
                        serde_json::json!({"command":format!("cargo test --marker=PRIVATE_{n}")}),
                    ),
            ))
            .unwrap();
    }
    relay
        .record_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "call-0",
            ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
        )))
        .unwrap();
    let before = relay
        .turn_context()
        .evidence(
            HarnessKind::Codex,
            TurnPhase::Replied,
            &ActivityFacts::default(),
            0,
        )
        .transcript_summary;
    drop(relay);
    let relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    let after = relay
        .turn_context()
        .evidence(
            HarnessKind::Codex,
            TurnPhase::Replied,
            &ActivityFacts::default(),
            0,
        )
        .transcript_summary;
    assert_eq!(before, after);
    assert!(!after.contains("PRIVATE_0"));
    assert!(!after.contains("PRIVATE_8"));
    assert!(!after.contains("<tool"));
    assert!(after.contains("Summarize this work"));
    assert!(after.contains("Preserved answer"));
}

#[test]
fn jev_user_boundary_moves_only_after_confirmed_steering_and_survives_reopen() {
    use mj_core::activity::{ActivityFacts, verdict::TurnPhase};
    use mj_core::config::HarnessKind;
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    let evidence = |relay: &DurableRelay| {
        relay.turn_context().evidence(
            HarnessKind::Codex,
            TurnPhase::Running,
            &ActivityFacts::default(),
            0,
        )
    };
    submit_relay(&mut relay, "active-prompt", prompt("ORIGINAL REQUEST"));
    relay.claim_pending_commands(true).unwrap();
    submit_relay(&mut relay, "queued-prompt", prompt("DELIVERED CORRECTION"));
    assert_eq!(evidence(&relay).user_prompt_tail, "ORIGINAL REQUEST");
    assert!(
        !evidence(&relay)
            .transcript_summary
            .contains("DELIVERED CORRECTION")
    );
    submit_relay(&mut relay, "cancel-command", RelayCommand::Cancel);
    relay.claim_pending_commands(true).unwrap();
    assert_eq!(evidence(&relay).user_prompt_tail, "ORIGINAL REQUEST");
    relay
        .record_command_completed(
            "cancel-command",
            RelayCommandOutcome::Steered {
                queued_command_id: "queued-prompt".into(),
            },
        )
        .unwrap();
    let before = evidence(&relay);
    assert_eq!(before.user_prompt_tail, "DELIVERED CORRECTION");
    assert!(before.transcript_summary.contains("DELIVERED CORRECTION"));
    assert!(!before.transcript_summary.contains("ORIGINAL REQUEST"));
    drop(relay);
    let relay = DurableRelay::open(temp.path(), SESSION, "test").unwrap();
    let after = evidence(&relay);
    assert_eq!(after.user_prompt_tail, before.user_prompt_tail);
    assert_eq!(after.transcript_summary, before.transcript_summary);
}

/// I1-11: a finished turn leaves its completed dispatch in the ledger until the
/// daemon acknowledges its events. That record is history, not work, so it
/// must not make `/clear` think the session is busy.
#[test]
fn clear_is_accepted_after_a_finished_turn_whose_events_are_unacknowledged() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = clearable_relay(temp.path());
    submit_relay(&mut relay, "finished-turn", prompt("Remember PINEAPPLE"));
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(
            "finished-turn",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    assert!(relay.snapshot.dispatches.contains_key("finished-turn"));
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Idle
    );
    let accepted = relay
        .submit_command("clear-request", RelayCommand::ClearContext)
        .unwrap();
    assert!(accepted.is_ok(), "{accepted:?}");
}

/// I1-13: a rejected `/clear` stays in the ledger until its events are
/// acknowledged. The session must accept prompts again at once instead of
/// saying the context is still being cleared.
#[test]
fn a_rejected_clear_does_not_block_later_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = clearable_relay(temp.path());
    assert!(
        relay
            .submit_command("clear-request", RelayCommand::ClearContext)
            .unwrap()
            .is_ok()
    );
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_rejected("clear-request", "restore model after clear failed")
        .unwrap();
    assert!(relay.snapshot.dispatches.contains_key("clear-request"));
    assert_eq!(relay.clear_context_started_at_ms(), None);
    let accepted = relay
        .submit_command("after-clear", prompt("still usable"))
        .unwrap();
    assert!(accepted.is_ok(), "{accepted:?}");
}

/// I1-11: the refusal still holds while a turn is running.
#[test]
fn clear_is_refused_while_a_turn_is_running() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = clearable_relay(temp.path());
    submit_relay(&mut relay, "running-turn", prompt("Write a story"));
    relay.claim_pending_commands(true).unwrap();
    let refused = relay
        .submit_command("clear-request", RelayCommand::ClearContext)
        .unwrap();
    assert!(refused.is_err());
}

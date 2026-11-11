use super::{test_support::*, *};
use mj_core::config::HarnessKind;

#[test]
fn handback_attempt_survives_restart_and_reminders_but_new_assignment_gets_a_check() {
    let root = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(root.path(), SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(HarnessKind::Codex);
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    submit_relay(
        &mut relay,
        "person-before-assignment",
        prompt("Top-level prompt must not start handback history."),
    );
    relay.claim_pending_commands(true).unwrap();
    let assistant = || {
        serde_json::from_value(serde_json::json!({
        "sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"Assistant progress must not be mirrored."}
    })).unwrap()
    };
    relay.record_session_update(assistant()).unwrap();
    assert!(relay.snapshot.handback_checks.context.is_none());
    finish_prompt(&mut relay, "person-before-assignment");
    let assignment = "subagent-spawn-original";
    submit_relay(
        &mut relay,
        assignment,
        prompt("Implement and validate the requested change."),
    );
    relay.claim_pending_commands(true).unwrap();
    relay.record_session_update(assistant()).unwrap();
    let instructions = relay.snapshot.handback_checks.context.as_ref().unwrap();
    assert_eq!(instructions.messages.len(), 1);
    assert!(
        instructions
            .messages
            .iter()
            .all(|message| message.role == "user")
    );
    assert!(
        !serde_json::to_string(instructions)
            .unwrap()
            .contains("Assistant progress")
    );
    let stale_snapshot = std::fs::read(root.path().join(RELAY_STATE_FILE)).unwrap();
    relay.set_continuation_enabled(false);
    assert!(
        relay
            .prepare_handback_check("Disabled continuation")
            .unwrap()
            .is_none()
    );
    relay.set_continuation_enabled(true);
    let (_, _, evidence) = relay
        .prepare_handback_check("Implementation still needs validation.")
        .unwrap()
        .unwrap();
    assert_eq!(
        evidence.completion.as_ref().unwrap().stop_reason,
        "end_turn"
    );
    assert!(evidence.completion.as_ref().unwrap().diagnostic.is_none());
    assert_eq!(
        evidence.phase,
        mj_core::activity::verdict::TurnPhase::Replied
    );
    assert_eq!(
        evidence
            .authorization
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .text,
        "Implementation still needs validation."
    );
    assert!(
        relay
            .prepare_handback_check("A second report")
            .unwrap()
            .is_none()
    );
    finish_prompt(&mut relay, assignment);
    drop(relay);
    // Force journal replay rather than letting the materialized snapshot prove durability.
    std::fs::write(root.path().join(RELAY_STATE_FILE), stale_snapshot).unwrap();
    let mut relay = DurableRelay::open(root.path(), SESSION, "test").unwrap();
    relay.set_turn_verdict_harness(HarnessKind::Codex);
    assert!(
        relay
            .prepare_handback_check("After restart")
            .unwrap()
            .is_none()
    );
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    let instructions = relay.snapshot.handback_checks.context.as_ref().unwrap();
    assert_eq!(
        instructions.messages.len(),
        1,
        "replay must retain instructions alone"
    );
    assert!(
        instructions
            .messages
            .iter()
            .all(|message| message.role == "user")
    );
    let completed_ordinal = relay
        .snapshot
        .turn_completion
        .as_ref()
        .unwrap()
        .completed_ordinal;
    let reminder = mj_core::subagent::handback_reminder_command_id(completed_ordinal);
    submit_relay(
        &mut relay,
        &reminder,
        RelayCommand::HandbackReminder {
            completed_command_id: assignment.into(),
            completed_ordinal,
        },
    );
    relay.claim_pending_commands(true).unwrap();
    assert!(
        relay
            .prepare_handback_check("After reminder")
            .unwrap()
            .is_none()
    );
    finish_prompt(&mut relay, &reminder);
    submit_relay(
        &mut relay,
        "subagent-input-followup",
        prompt("Also validate the new case."),
    );
    relay.claim_pending_commands(true).unwrap();
    finish_prompt(&mut relay, "subagent-input-followup");
    submit_relay(
        &mut relay,
        "person-after-assignment",
        prompt("Keep the parent's instructions in scope."),
    );
    relay.claim_pending_commands(true).unwrap();
    relay.record_session_update(assistant()).unwrap();
    let instructions = relay.snapshot.handback_checks.context.as_ref().unwrap();
    assert_eq!(instructions.messages.len(), 3);
    assert!(
        instructions
            .messages
            .iter()
            .all(|message| message.role == "user")
    );
    assert!(
        relay
            .prepare_handback_check("Follow-up report")
            .unwrap()
            .is_some()
    );
}

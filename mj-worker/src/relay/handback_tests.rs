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
    // Hard-won: e44bee684: a report over 2,048 bytes broke the proxy's tail bound, so the check was spent unchecked
    let long_report = format!("Follow-up report. {}Remaining: none.", "é".repeat(3000));
    let (_, _, evidence) = relay
        .prepare_handback_check(&long_report)
        .unwrap()
        .expect("follow-up assignment is checked");
    assert!(evidence.assistant_text_tail.len() <= mj_core::activity::verdict::ASSISTANT_TEXT_BYTES);
    assert!(evidence.assistant_text_tail.ends_with("Remaining: none."));
    finish_prompt(&mut relay, "person-after-assignment");

    // Hard-won: 9130f139e: parent follow-ups delivered through the mailbox bypassed the handback check
    for hook in [true, false] {
        use mj_core::mailbox::{MailboxDeliveryPath, MailboxEvent, MailboxEventBody};
        let event = MailboxEvent {
            key: format!("parent-followup-{hook}"),
            source: "parent".into(),
            wake: true,
            created_at_ms: 1,
            body: MailboxEventBody::ParentMessage {
                text: format!("Validate mailbox delivery {hook}"),
            },
        };
        let stale_snapshot = std::fs::read(root.path().join(RELAY_STATE_FILE)).unwrap();
        relay
            .record_observation(RelayObservation::CommandQueued {
                command_id: format!("deliver-{hook}"),
                command: RelayCommand::DeliverMailboxEvent {
                    event: event.clone(),
                },
                created_at_ms: 1,
            })
            .unwrap();
        // Queuing alone must not create an assignment.
        assert_ne!(
            relay.snapshot.handback_checks.assignment_id.as_deref(),
            Some(event.handback_assignment().unwrap().0.as_str())
        );
        if hook {
            relay
                .record_observation(RelayObservation::MailboxEventsDelivered {
                    event_keys: vec![event.key.clone()],
                    path: MailboxDeliveryPath::ToolHook,
                    prompt_command_id: None,
                    hook_event: Some("PostToolUse".into()),
                    events: vec![event.clone()],
                    lease_id: None,
                    steering: None,
                })
                .unwrap();
        } else {
            relay
                .record_observation(RelayObservation::CommandQueued {
                    command_id: "mailbox-wake-followup".into(),
                    command: RelayCommand::MailboxWake {
                        events: vec![event.clone()],
                    },
                    created_at_ms: 1,
                })
                .unwrap();
            relay.claim_pending_commands(true).unwrap();
            finish_prompt(&mut relay, "mailbox-wake-followup");
        }
        let expected_id = event.handback_assignment().unwrap().0;
        assert_eq!(
            relay.snapshot.handback_checks.assignment_id.as_ref(),
            Some(&expected_id)
        );
        let (_, _, evidence) = relay
            .prepare_handback_check("Mailbox report")
            .unwrap()
            .unwrap();
        assert!(
            serde_json::to_string(&evidence)
                .unwrap()
                .contains(&format!("Validate mailbox delivery {hook}"))
        );
        assert!(relay.prepare_handback_check("Duplicate").unwrap().is_none());
        drop(relay);
        std::fs::write(root.path().join(RELAY_STATE_FILE), stale_snapshot).unwrap();
        relay = DurableRelay::open(root.path(), SESSION, "test").unwrap();
        relay.set_turn_verdict_harness(HarnessKind::Codex);
        assert_eq!(
            relay.snapshot.handback_checks.assignment_id.as_ref(),
            Some(&expected_id)
        );
        assert!(relay.prepare_handback_check("Restart").unwrap().is_none());
        let peer = MailboxEvent {
            key: format!("peer-{hook}"),
            body: MailboxEventBody::PlainText {
                text: "Peer instructions are not an assignment".into(),
            },
            wake: false,
            ..event
        };
        submit_relay(
            &mut relay,
            &format!("peer-delivery-{hook}"),
            RelayCommand::DeliverMailboxEvent {
                event: peer.clone(),
            },
        );
        relay
            .record_observation(RelayObservation::MailboxEventsDelivered {
                event_keys: vec![peer.key.clone()],
                path: MailboxDeliveryPath::ToolHook,
                prompt_command_id: None,
                hook_event: Some("PostToolUse".into()),
                events: vec![peer],
                lease_id: None,
                steering: None,
            })
            .unwrap();
        assert_eq!(
            relay.snapshot.handback_checks.assignment_id.as_ref(),
            Some(&expected_id)
        );
        assert!(
            !serde_json::to_string(&relay.snapshot.handback_checks.context)
                .unwrap()
                .contains("Peer instructions")
        );
        assert!(
            relay
                .prepare_handback_check("Peer report")
                .unwrap()
                .is_none()
        );
    }
}

use mj_core::relay::capacity::*;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{test_support::*, *};
    use agent_client_protocol::schema::v1::{ContentChunk, TextContent};

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
    }

    fn open(root: &std::path::Path) -> DurableRelay {
        let mut relay = DurableRelay::open(root, SESSION, "test").unwrap();
        relay.set_background_work_policy(BackgroundWorkPolicy::CodexExecCards);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: vec![],
            })
            .unwrap();
        relay
    }

    fn start(relay: &mut DurableRelay, id: &str) {
        submit_relay(relay, id, prompt("work"));
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            id
        );
    }

    fn finish(relay: &mut DurableRelay, id: &str, text: &str, stop: &str) -> u64 {
        relay.record_session_update(chunk(text)).unwrap();
        relay
            .record_command_completed(
                id,
                RelayCommandOutcome::Prompt {
                    stop_reason: stop.into(),
                    usage: None,
                },
            )
            .unwrap()
    }

    #[test]
    fn capacity_retries_back_off_and_reset_after_a_successful_turn() {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        start(&mut relay, "original-prompt");
        let mut id = "original-prompt".to_owned();
        for (index, minutes) in [1, 2, 4, 8, 16, 16].into_iter().enumerate() {
            let ordinal = finish(&mut relay, &id, CAPACITY_MESSAGE, "EndTurn");
            let retry = relay.operational_state().capacity_retry.unwrap();
            assert_eq!(retry.attempt, index as u32 + 1);
            let event = relay
                .events_after(ordinal - 1, &relay.digest_at(ordinal - 1).unwrap().unwrap())
                .unwrap()
                .into_iter()
                .find(|event| event.ordinal == ordinal)
                .unwrap();
            assert_eq!(retry.retry_at_ms - event.recorded_at_ms, minutes * 60_000);
            assert!(
                !relay
                    .submit_due_capacity_retry(retry.retry_at_ms - 1)
                    .unwrap()
            );
            assert!(relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
            assert!(!relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
            let commands = relay.claim_pending_commands(true).unwrap();
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].command, prompt("Continue"));
            id = commands[0].command_id.clone();
        }
        finish(&mut relay, &id, "Done", "EndTurn");
        assert!(relay.capacity_retry_deadline().is_none());
        start(&mut relay, "fresh-prompt");
        finish(&mut relay, "fresh-prompt", CAPACITY_MESSAGE, "EndTurn");
        assert_eq!(relay.operational_state().capacity_retry.unwrap().attempt, 1);
    }

    #[test]
    fn capacity_retry_survives_reopening_without_duplicate_submission() {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        start(&mut relay, "original-prompt");
        finish(&mut relay, "original-prompt", CAPACITY_MESSAGE, "EndTurn");
        let retry = relay.operational_state().capacity_retry.unwrap();
        drop(relay);
        let mut relay = open(root.path());
        assert_eq!(
            relay.operational_state().capacity_retry,
            Some(retry.clone())
        );
        assert!(
            !relay
                .submit_due_capacity_retry(retry.retry_at_ms - 1)
                .unwrap()
        );
        assert!(relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
        drop(relay);
        let mut relay = open(root.path());
        assert!(!relay.submit_due_capacity_retry(i64::MAX).unwrap());
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    }

    #[test]
    fn explicit_input_and_control_cancel_waiting_capacity_retries() {
        for command in [
            prompt("resume myself"),
            RelayCommand::CancelTurn,
            RelayCommand::Cancel,
            RelayCommand::SetConfig {
                key: "model".into(),
                value: "other".into(),
            },
            RelayCommand::SetSessionMode {
                mode_id: "default".into(),
            },
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut relay = open(root.path());
            start(&mut relay, "original-prompt");
            finish(&mut relay, "original-prompt", CAPACITY_MESSAGE, "EndTurn");
            submit_relay(&mut relay, "external-command", command);
            assert!(!relay.submit_due_capacity_retry(i64::MAX).unwrap());
            assert!(relay.capacity_retry_deadline().is_none());
        }
    }

    #[test]
    fn user_input_queued_before_capacity_completion_prevents_retry() {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        start(&mut relay, "original-prompt");
        submit_relay(&mut relay, "external-prompt", prompt("different task"));
        finish(&mut relay, "original-prompt", CAPACITY_MESSAGE, "EndTurn");
        assert!(relay.capacity_retry_deadline().is_none());
    }

    #[test]
    fn routine_checkpoint_holds_dispatch_without_cancelling_capacity_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        start(&mut relay, "original-prompt");
        finish(&mut relay, "original-prompt", CAPACITY_MESSAGE, "EndTurn");
        let retry = relay.operational_state().capacity_retry.unwrap();
        ready_checkpoint(&mut relay, "routine-checkpoint");
        assert!(!relay.submit_due_capacity_retry(i64::MAX).unwrap());
        assert_eq!(
            relay.operational_state().capacity_retry,
            Some(retry.clone())
        );
        submit_relay(
            &mut relay,
            "release-checkpoint",
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: "routine-checkpoint".into(),
            },
        );
        assert!(relay.submit_due_capacity_retry(retry.retry_at_ms).unwrap());
    }

    #[test]
    fn separate_sessions_recover_independently_and_wait_for_harness_readiness() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let mut a = open(left.path());
        let mut b = open(right.path());
        for relay in [&mut a, &mut b] {
            start(relay, "original-prompt");
            finish(relay, "original-prompt", CAPACITY_MESSAGE, "EndTurn");
        }
        submit_relay(&mut a, "cancel-recovery", RelayCommand::CancelTurn);
        b.record_observation(RelayObservation::SessionRestarted)
            .unwrap();
        assert!(!a.submit_due_capacity_retry(i64::MAX).unwrap());
        assert!(!b.submit_due_capacity_retry(i64::MAX).unwrap());
        b.record_observation(RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
        assert!(b.submit_due_capacity_retry(i64::MAX).unwrap());
    }

    #[test]
    fn rejected_turns_cannot_leak_capacity_text_into_a_later_empty_response() {
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        start(&mut relay, "original-prompt");
        relay
            .record_session_update(chunk(CAPACITY_MESSAGE))
            .unwrap();
        relay
            .record_command_rejected("original-prompt", "transport failed")
            .unwrap();
        start(&mut relay, "following-prompt");
        relay
            .record_command_completed(
                "following-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "EndTurn".into(),
                    usage: None,
                },
            )
            .unwrap();
        assert!(relay.capacity_retry_deadline().is_none());
    }

    #[test]
    fn structured_capacity_errors_and_historical_text_have_distinct_effects() {
        assert!(capacity_error(
            &agent_client_protocol::Error::new(-32603, "failed")
                .data(serde_json::json!({"codex_error_info": "server_overloaded"}))
        ));
        assert!(!capacity_error(&agent_client_protocol::Error::new(
            -32603,
            CAPACITY_MESSAGE
        )));
        let root = tempfile::tempdir().unwrap();
        let mut relay = open(root.path());
        start(&mut relay, "original-prompt");
        // Historical/imported observations are not live capacity classification.
        relay
            .record_observation(RelayObservation::SessionUpdate {
                update: Box::new(chunk(CAPACITY_MESSAGE)),
            })
            .unwrap();
        relay
            .record_command_completed(
                "original-prompt",
                RelayCommandOutcome::Prompt {
                    stop_reason: "EndTurn".into(),
                    usage: None,
                },
            )
            .unwrap();
        assert!(relay.capacity_retry_deadline().is_none());
        start(&mut relay, "structured-prompt");
        finish(&mut relay, "structured-prompt", "", CAPACITY_STOP_REASON);
        assert!(relay.capacity_retry_deadline().is_some());
    }

    #[test]
    fn large_earlier_messages_do_not_hide_the_final_capacity_response() {
        let mut response = CapacityResponse::default();
        response.observe(&SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("x".repeat(70_000))))
                .message_id("commentary"),
        ));
        assert!(!response.at_capacity());
        response.observe(&SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(CAPACITY_MESSAGE)))
                .message_id("final"),
        ));
        assert!(response.at_capacity());
        response.observe(&SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("Actually, done.")))
                .message_id("later-final"),
        ));
        assert!(!response.at_capacity());
    }

    #[test]
    fn capacity_classification_requires_the_exact_final_message_and_a_non_cancelled_codex_turn() {
        let mut response = CapacityResponse::default();
        response.observe(&chunk("Selected model is at capacity."));
        response.observe(&chunk(" Please try a different model.\n\n"));
        assert!(response.at_capacity());
        response.observe(&chunk(" Here is how to handle that error."));
        assert!(!response.at_capacity());
        for (text, stop, codex) in [
            (CAPACITY_MESSAGE, "Cancelled", true),
            (CAPACITY_MESSAGE, "EndTurn", false),
            (
                "Example: Selected model is at capacity. Please try a different model.",
                "EndTurn",
                true,
            ),
            ("Network error", "error", true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut relay = open(root.path());
            if !codex {
                relay.set_background_work_policy(BackgroundWorkPolicy::HostedTerminals);
            }
            start(&mut relay, "original-prompt");
            finish(&mut relay, "original-prompt", text, stop);
            assert!(relay.capacity_retry_deadline().is_none());
        }
    }
}

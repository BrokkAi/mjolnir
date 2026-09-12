use agent_client_protocol::schema::v1::*;
use mj_core::config::HarnessKind;
use mj_core::relay::RELAY_EVENT_GENESIS_DIGEST;
use mj_core::relay::snapshot::*;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::test_support::*;
    use crate::relay::{
        DurableRelay, RELAY_COMMAND_BYTE_BUDGET, RELAY_EVENT_BYTE_BUDGET, RELAY_STATE_BYTE_BUDGET,
        RelayErrorCode, RelayProtocolError, RelayRequest, RelayResponseBody,
    };

    #[test]
    fn cancel_turn_is_reserved_for_relay_protocol_v7() {
        assert_eq!(RelayCommand::CancelTurn.minimum_protocol(), 7);
        assert_eq!(RelayCommand::Cancel.minimum_protocol(), 1);
    }

    /// Every way a session can still be holding work, each on its own, plus
    /// the one state in which replacing its worker destroys nothing.
    #[test]
    fn a_session_is_quiet_only_when_nothing_it_owns_is_in_flight() {
        let temp = tempfile::tempdir().unwrap();
        let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let mut quiet = relay.operational_state();
        assert!(!quiet.is_quiet(), "startup is still in flight");
        quiet.acp_ready = Some(true);
        assert!(
            quiet.is_quiet(),
            "a ready idle relay owns nothing: {quiet:?}"
        );

        type MakeBusy = fn(&mut RelayOperationalState);
        let busy: Vec<(&str, MakeBusy)> = vec![
            (
                "a prompt is running",
                (|state| state.execution = RelayExecutionState::Running),
            ),
            (
                "a prompt is in flight",
                (|state| {
                    state.active_prompt = Some(ActiveRelayPrompt {
                        command_id: "prompt-1".into(),
                        created_at_ms: 1,
                        started_at_ms: 2,
                    });
                }),
            ),
            (
                "the harness started a turn of its own",
                (|state| {
                    state.harness_turn = Some(HarnessTurn { started_at_ms: 1 });
                }),
            ),
            (
                "a prompt is queued behind the current one",
                (|state| {
                    state.queued_prompts = vec![QueuedRelayPrompt {
                        command_id: "prompt-2".into(),
                        created_at_ms: 1,
                    }];
                }),
            ),
            (
                "a user shell is open",
                (|state| {
                    state.active_user_shells = vec![ActiveUserShell {
                        command_id: "shell-1".into(),
                        command: "top".into(),
                        created_at_ms: 1,
                        started_at_ms: Some(1),
                    }];
                }),
            ),
            (
                "an agent terminal is live",
                (|state| {
                    state.active_agent_terminals = vec![ActiveAgentTerminal {
                        terminal_id: "term-1".into(),
                        command: "sleep 600".into(),
                        started_at_ms: 1,
                    }];
                }),
            ),
            (
                "the agent left a command running",
                (|state| {
                    state.background_commands = vec![BackgroundCommand {
                        id: "task-1".into(),
                        started_at_ms: 1,
                        command: "sleep 600".into(),
                        can_stop: false,
                    }];
                }),
            ),
            (
                "a foreground tool is still in progress",
                (|state| state.foreground_tool_started_at_ms = Some(1)),
            ),
            (
                "a checkpoint barrier is waiting",
                (|state| state.checkpoint_barrier = Some("checkpoint-1".into())),
            ),
        ];
        for (reason, make_busy) in busy {
            let mut state = quiet.clone();
            make_busy(&mut state);
            assert!(!state.is_quiet(), "{reason}");
        }
    }

    #[test]
    fn relay_operational_state_tracks_mutable_acp_options_and_commands() {
        use agent_client_protocol::schema::v1::{
            AvailableCommandsUpdate, ConfigOptionUpdate, CurrentModeUpdate,
            SessionConfigSelectOption, SessionMode, SessionModeState,
        };

        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let option = SessionConfigOption::select(
            "thinking",
            "Thinking",
            "on",
            vec![SessionConfigSelectOption::new("on", "On")],
        );
        relay
            .record_session_update(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                vec![option.clone()],
            )))
            .unwrap();
        relay
            .record_observation(RelayObservation::SessionModesConfigured {
                modes: Some(SessionModeState::new(
                    "default",
                    vec![
                        SessionMode::new("default", "Default"),
                        SessionMode::new("plan", "Plan"),
                    ],
                )),
            })
            .unwrap();
        relay
            .record_session_update(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                "plan",
            )))
            .unwrap();
        relay
            .record_session_update(SessionUpdate::AvailableCommandsUpdate(
                AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                    "review",
                    "Review the current work",
                )]),
            ))
            .unwrap();

        let state = relay.operational_state();
        assert_eq!(state.config_options, vec![option]);
        assert_eq!(state.config["mode"], "plan");
        assert_eq!(
            state.modes.unwrap().current_mode_id.to_string(),
            "plan",
            "current_mode_update keeps the legacy catalogue synchronized"
        );
        assert_eq!(state.available_commands[0].name, "review");
    }

    #[test]
    fn snapshots_without_legacy_modes_still_deserialize() {
        let snapshot = RelaySnapshot::new(SESSION.into());
        let mut encoded = serde_json::to_value(snapshot).unwrap();
        encoded.as_object_mut().unwrap().remove("modes");

        let restored: RelaySnapshot = serde_json::from_value(encoded).unwrap();

        assert_eq!(restored.modes, None);
    }

    #[test]
    fn native_session_readiness_requires_current_acp_session() {
        let mut state = RelaySnapshot::new(SESSION.into()).operational_state();
        state.native_session_id = Some("restored-native-session".into());
        state.acp_ready = Some(false);
        assert!(!state.native_session_is_ready());

        state.acp_ready = Some(true);
        assert!(state.native_session_is_ready());

        state.execution = RelayExecutionState::Closed;
        assert!(!state.native_session_is_ready());
    }

    #[test]
    fn legacy_operational_state_without_acp_readiness_is_ready() {
        let mut state = RelaySnapshot::new(SESSION.into()).operational_state();
        state.native_session_id = Some("legacy-native-session".into());
        assert_eq!(state.acp_ready, None);
        let mut encoded = serde_json::to_value(state).unwrap();
        encoded.as_object_mut().unwrap().remove("acp_ready");

        let restored: RelayOperationalState = serde_json::from_value(encoded).unwrap();

        assert_eq!(restored.acp_ready, None);
        assert!(restored.native_session_is_ready());
    }

    #[test]
    fn kimi_replacement_requires_current_background_work_knowledge() {
        let mut state = RelaySnapshot::new(SESSION.into()).operational_state();
        state.native_session_id = Some("native-session".into());
        state.acp_ready = Some(true);

        assert!(state.is_quiet());
        assert!(!state.safe_to_replace(HarnessKind::Codex));
        assert!(
            !state.safe_to_replace(HarnessKind::Kimi),
            "an older Kimi worker cannot prove provider tasks are absent"
        );

        state.background_work_known = Some(false);
        assert!(!state.is_quiet());
        assert!(!state.safe_to_replace(HarnessKind::Kimi));

        state.background_work_known = Some(true);
        assert!(state.is_quiet());
        assert!(state.safe_to_replace(HarnessKind::Kimi));
    }

    #[test]
    fn kimi_checkpoint_requires_known_empty_background_work() {
        let mut state = RelaySnapshot::new(SESSION.into()).operational_state();
        assert!(!state.safe_for_checkpoint(HarnessKind::Codex));
        assert!(
            !state.safe_for_checkpoint(HarnessKind::Kimi),
            "an older Kimi worker cannot prove provider tasks are absent"
        );

        assert!(
            state
                .checkpoint_background_blocker(HarnessKind::Kimi)
                .unwrap()
                .contains("not reported")
        );
        state.background_work_known = Some(false);
        assert!(!state.safe_for_checkpoint(HarnessKind::Kimi));

        assert!(
            state
                .checkpoint_background_blocker(HarnessKind::Kimi)
                .unwrap()
                .contains("not synchronized")
        );
        state.background_work_known = Some(true);
        assert!(state.safe_for_checkpoint(HarnessKind::Kimi));
        state.background_commands.push(BackgroundCommand {
            id: "kimi:agent-1".into(),
            started_at_ms: 1,
            command: "background agent".into(),
            can_stop: false,
        });
        assert!(!state.safe_for_checkpoint(HarnessKind::Kimi));
    }

    #[test]
    fn initializing_operational_state_is_not_quiet() {
        let mut state = RelaySnapshot::new(SESSION.into()).operational_state();
        state.acp_ready = Some(false);

        assert!(!state.is_quiet());
    }

    /// Transcript observations skip the staged snapshot copy and its budget
    /// checks, which is only sound while applying one really moves nothing but
    /// the frontier. Anything that can grow the snapshot must classify as a
    /// state move so its budget is still checked before it is journaled.
    #[test]
    fn transcript_observations_move_nothing_but_the_frontier() {
        use agent_client_protocol::schema::v1::{
            AvailableCommandsUpdate, ContentBlock, ContentChunk,
        };

        let transcript = [
            RelayObservation::Warning {
                message: "warned".into(),
            },
            RelayObservation::Notice {
                message: "noticed".into(),
            },
            RelayObservation::TerminalOutput {
                terminal_id: "terminal-1".into(),
                output: "output".into(),
                truncated: false,
                exit_code: Some(0),
                signal: None,
            },
            RelayObservation::PermissionAutoApproved {
                option_id: "allow".into(),
                option_name: "Allow".into(),
            },
            RelayObservation::ElicitationRequested {
                request: mj_core::elicitation::ElicitationRequest {
                    id: "elicitation-1".into(),
                    message: "confirm".into(),
                    title: None,
                    description: None,
                    fields: Vec::new(),
                },
            },
            RelayObservation::ElicitationResolved {
                elicitation_id: "elicitation-1".into(),
                action: "accept".into(),
            },
            RelayObservation::ElicitationsCleared,
            RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from("streamed"),
                ))),
            },
        ];
        for observation in transcript {
            assert!(
                !observation_changes_state(&observation),
                "{observation:?} is classified as a state move"
            );
            let mut snapshot = RelaySnapshot::new(SESSION.to_owned());
            let event = RelayEvent {
                format: RELAY_EVENT_FORMAT_V1,
                ordinal: 1,
                previous_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
                digest: String::new(),
                recorded_at_ms: 7,
                command_id: None,
                observation,
            };
            let event = RelayEvent {
                digest: relay_event_digest(&event).unwrap(),
                ..event
            };
            let mut expected = snapshot.clone();
            expected.latest_ordinal = event.ordinal;
            expected.latest_digest.clone_from(&event.digest);
            apply_relay_event(&mut snapshot, &event).unwrap();
            assert_eq!(
                snapshot, expected,
                "{:?} changed durable state",
                event.observation
            );
        }

        for observation in [
            RelayObservation::CommandQueued {
                command_id: "queued-command".into(),
                command: prompt("grow the snapshot"),
                created_at_ms: 7,
            },
            RelayObservation::SessionUpdate {
                update: Box::new(SessionUpdate::AvailableCommandsUpdate(
                    AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                        "review",
                        "Review the current work",
                    )]),
                )),
            },
            // A harness-initiated turn moves execution state, and a restart
            // ends one, so all three must be applied through a staged snapshot
            // rather than appended as transcript-only frontier moves.
            RelayObservation::HarnessTurnStarted { started_at_ms: 7 },
            RelayObservation::HarnessTurnSettled {
                origin: Some("task-notification".into()),
                prompt_in_flight: false,
            },
            RelayObservation::SessionRestarted,
        ] {
            assert!(
                observation_changes_state(&observation),
                "{observation:?} can grow the snapshot and must be budget-checked"
            );
        }
    }

    #[test]
    fn v1_events_round_trip_byte_identically_and_v2_omits_the_chain() {
        let observation = || RelayObservation::Warning {
            message: "hi".into(),
        };

        // v1: no `format` key on the wire, keeps previous_digest, chains to its
        // cursor. Existing journals stay byte-for-byte identical.
        let mut v1 = RelayEvent {
            format: RELAY_EVENT_FORMAT_V1,
            ordinal: 1,
            previous_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
            digest: String::new(),
            recorded_at_ms: 42,
            command_id: None,
            observation: observation(),
        };
        v1.digest = relay_event_digest(&v1).unwrap();
        let v1_json = serde_json::to_string(&v1).unwrap();
        assert!(
            !v1_json.contains("\"format\""),
            "v1 must not write a format key: {v1_json}"
        );
        assert!(v1_json.contains("previous_digest"));
        validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &v1).unwrap();

        // v2: tags its format, carries no chain link, and self-validates
        // regardless of the cursor digest.
        let mut v2 = RelayEvent {
            format: RELAY_EVENT_FORMAT_V2,
            ordinal: 1,
            previous_digest: String::new(),
            digest: String::new(),
            recorded_at_ms: 42,
            command_id: None,
            observation: observation(),
        };
        v2.digest = relay_event_digest(&v2).unwrap();
        let v2_json = serde_json::to_string(&v2).unwrap();
        assert!(
            v2_json.contains("\"format\":2"),
            "v2 must tag its format: {v2_json}"
        );
        assert!(
            !v2_json.contains("previous_digest"),
            "v2 must not write a chain link: {v2_json}"
        );
        validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &v2).unwrap();
        validate_relay_event(0, &"a".repeat(64), &v2)
            .expect("a v2 event has no in-record link, so any cursor digest is accepted");

        // Same ordinal + content, different format → different digest (domain
        // separation + payload), so v1 and v2 never collide.
        assert_ne!(v1.digest, v2.digest);

        // Round-trip both, and confirm an old record with no `format` key reads
        // as v1.
        let v1_back: RelayEvent = serde_json::from_str(&v1_json).unwrap();
        assert_eq!(v1_back, v1);
        let v2_back: RelayEvent = serde_json::from_str(&v2_json).unwrap();
        assert_eq!(v2_back, v2);
        assert_eq!(v2_back.previous_digest, "");
        let legacy: RelayEvent =
            serde_json::from_str(r#"{"ordinal":1,"previous_digest":"","digest":"x","recorded_at_ms":0,"observation":{"type":"warning","data":{"message":"m"}}}"#)
                .unwrap();
        assert_eq!(legacy.format, RELAY_EVENT_FORMAT_V1);
    }

    #[test]
    fn queue_entries_written_before_config_changes_still_load() {
        let stored: StoredQueuedRelayCommand = serde_json::from_value(serde_json::json!({
            "command_id": "queued-1",
            "prompt": [{"type": "text", "text": "hello"}],
            "created_at_ms": 7,
        }))
        .unwrap();
        assert!(matches!(
            stored.payload,
            StoredQueuedRelayPayload::Prompt { .. }
        ));

        let config = StoredQueuedRelayCommand {
            command_id: "queued-2".into(),
            payload: StoredQueuedRelayPayload::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            },
            created_at_ms: 8,
        };
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(encoded["key"], "model");
        assert_eq!(
            serde_json::from_value::<StoredQueuedRelayCommand>(encoded).unwrap(),
            config
        );
    }

    #[test]
    fn oversized_commands_are_rejected_before_journaling() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(relay_request(
            "oversized-command",
            RelayRequest::Submit {
                command_id: "oversized-command".into(),
                command: prompt(&"x".repeat(RELAY_COMMAND_BYTE_BUDGET)),
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
        assert_eq!(relay.latest_ordinal(), 0);
    }

    #[test]
    fn truncate_start_keeps_the_tail_and_discloses_the_drop() {
        let mut short = "abcdefghij".to_owned();
        assert!(!truncate_start_with_marker(&mut short, 100));
        assert_eq!(short, "abcdefghij");

        let mut long = "abcdefghij".to_owned();
        assert!(truncate_start_with_marker(&mut long, 4));
        assert!(
            long.starts_with("[mj dropped "),
            "the drop must be disclosed: {long:?}"
        );
        assert!(long.ends_with("ghij"), "the tail must be kept: {long:?}");
    }

    #[test]
    fn oversized_observations_are_truncated_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();

        let ordinal = relay
            .record_observation(RelayObservation::Warning {
                message: "x".repeat(RELAY_EVENT_BYTE_BUDGET),
            })
            .expect("an oversized observation is recorded, not rejected");
        assert_eq!(ordinal, 1);
        assert_eq!(relay.latest_ordinal(), 1);

        let replayed = relay
            .events_after(0, crate::relay::RELAY_EVENT_GENESIS_DIGEST)
            .unwrap();
        let recorded = &replayed[0];
        let RelayObservation::Warning { message } = &recorded.observation else {
            panic!(
                "expected the truncated warning, found {:?}",
                recorded.observation
            );
        };
        assert!(
            message.starts_with("xxxx"),
            "the head of the payload is kept"
        );
        assert!(message.contains("[mj truncated"), "truncation is disclosed");
        assert!(serde_json::to_vec(recorded).unwrap().len() <= RELAY_EVENT_BYTE_BUDGET);
    }

    /// The journal is append-only, so what a recorded edit costs is what it
    /// costs forever. It records the patch, not two copies of the file.
    #[test]
    fn a_recorded_edit_journals_a_patch_rather_than_the_whole_file() {
        use agent_client_protocol::schema::v1::{Diff, ToolCall, ToolCallContent};

        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let old_text = (0..4_000)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let new_text = old_text.replace("line 2000\n", "line 2000 edited\n");
        let mut diff = Diff::new("/repo/src/main.rs", new_text);
        diff.old_text = Some(old_text.clone());

        relay
            .record_session_update(SessionUpdate::ToolCall(
                ToolCall::new("call-1", "Edit files").content(vec![ToolCallContent::Diff(diff)]),
            ))
            .unwrap();

        let replayed = relay
            .events_after(0, crate::relay::RELAY_EVENT_GENESIS_DIGEST)
            .unwrap();
        let recorded = &replayed[0];
        let RelayObservation::SessionUpdate { update } = &recorded.observation else {
            panic!(
                "expected a session update, found {:?}",
                recorded.observation
            );
        };
        let SessionUpdate::ToolCall(call) = update.as_ref() else {
            panic!("expected a tool call");
        };
        let [ToolCallContent::Diff(diff)] = call.content.as_slice() else {
            panic!("expected one diff");
        };
        assert_eq!(diff.old_text, None, "the old copy is not journalled");
        assert_eq!(diff.new_text, "", "the new copy is not journalled");
        let patch = mj_core::diff::patch_of(diff);
        assert_eq!((patch.insertions, patch.deletions), (1, 1));
        assert!(patch.text.contains("+line 2000 edited\n"));
        assert!(
            serde_json::to_vec(recorded).unwrap().len() * 20 < old_text.len(),
            "a one-line edit still cost a copy of the file"
        );
    }

    #[test]
    fn operational_state_is_payload_free_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        submit_relay(
            &mut relay,
            "secret-prompt",
            prompt("payload-that-must-not-be-in-operational-state"),
        );
        let encoded = serde_json::to_vec(&relay.operational_state()).unwrap();
        assert!(encoded.len() <= RELAY_STATE_BYTE_BUDGET);
        let encoded = String::from_utf8(encoded).unwrap();
        assert!(encoded.contains("secret-prompt"));
        assert!(!encoded.contains("payload-that-must-not-be-in-operational-state"));

        let half_budget = RELAY_STATE_BYTE_BUDGET / 2;
        relay
            .record_observation(RelayObservation::ConfigurationUpdated {
                key: "large-one".into(),
                value: "a".repeat(half_budget),
            })
            .unwrap();
        let before = relay.latest_ordinal();
        let error = relay
            .record_observation(RelayObservation::ConfigurationUpdated {
                key: "large-two".into(),
                value: "b".repeat(half_budget),
            })
            .unwrap_err();
        assert!(error.to_string().contains("operational state is too large"));
        assert_eq!(relay.latest_ordinal(), before);
    }

    #[test]
    fn event_chain_detects_cursor_and_body_desynchronization() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::Warning {
                message: "authentic".into(),
            })
            .unwrap();
        let event = retained_events(&relay)[0].clone();
        validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &event).unwrap();

        let mut tampered = event;
        tampered.observation = RelayObservation::Warning {
            message: "tampered".into(),
        };
        assert!(validate_relay_event(0, RELAY_EVENT_GENESIS_DIGEST, &tampered).is_err());

        let mismatch = relay.handle(relay_request(
            "attach-wrong-digest",
            RelayRequest::Attach {
                after_ordinal: 0,
                after_digest: "a".repeat(64),
            },
        ));
        assert!(matches!(
            mismatch.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Desynchronized,
                    ..
                }
            }
        ));
    }
}

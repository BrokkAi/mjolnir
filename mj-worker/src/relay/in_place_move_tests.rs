use super::test_support::*;
use super::*;

fn configured(temp: &tempfile::TempDir) -> DurableRelay {
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "native-session".into(),
            native_continuity_lost: false,
            resumed: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    relay
        .record_observation(RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    relay
}

fn prompt_command(text: &str) -> RelayCommand {
    RelayCommand::Prompt {
        prompt: vec![ContentBlock::from(text)],
    }
}

fn req(id: &str, request: RelayRequest) -> RelayRequestEnvelope {
    RelayRequestEnvelope {
        request_id: id.into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request,
    }
}

#[test]
fn in_place_move_hold_is_atomic_idempotent_and_enforces_store_id_on_all_commands() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

    // Start one prompt so subsequent prompts stay queued.
    let resp = relay.handle(req(
        "r-active",
        RelayRequest::Submit {
            command_id: "cmd-active".into(),
            command: prompt_command("active turn"),
        },
    ));
    assert!(matches!(resp.body, RelayResponseBody::Ok { .. }));
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);

    // Queue two pending prompts.
    for (req_id, cmd_id, text) in [
        ("r-q1", "prompt-1", "queued 1"),
        ("r-q2", "prompt-2", "queued 2"),
    ] {
        let resp = relay.handle(req(
            req_id,
            RelayRequest::Submit {
                command_id: cmd_id.into(),
                command: prompt_command(text),
            },
        ));
        assert!(
            matches!(resp.body, RelayResponseBody::Ok { .. }),
            "expected Ok for {req_id}, got {:?}",
            resp.body
        );
    }

    // Hold dispatch: captures exact pending IDs.
    let hold_resp = relay.handle(req(
        "r-hold-1",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
        },
    ));
    match hold_resp.body {
        RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::InPlaceMoveDispatchHeld {
                    captured_pending_command_ids,
                },
        } => {
            assert_eq!(captured_pending_command_ids, vec!["prompt-1", "prompt-2"]);
        }
        other => panic!("expected InPlaceMoveDispatchHeld, got {other:?}"),
    }

    // Another prompt arrives while held; repeating Hold with the same operation_id
    // must idempotently return the originally captured IDs.
    let resp = relay.handle(req(
        "r-q3",
        RelayRequest::Submit {
            command_id: "prompt-3".into(),
            command: prompt_command("queued 3"),
        },
    ));
    assert!(matches!(resp.body, RelayResponseBody::Ok { .. }));

    let hold_repeat = relay.handle(req(
        "r-hold-2",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
        },
    ));
    match hold_repeat.body {
        RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::InPlaceMoveDispatchHeld {
                    captured_pending_command_ids,
                },
        } => {
            assert_eq!(captured_pending_command_ids, vec!["prompt-1", "prompt-2"]);
        }
        other => panic!("expected idempotent InPlaceMoveDispatchHeld, got {other:?}"),
    }

    // A different operation_id is refused while move-op-1 holds dispatch.
    let other_op = relay.handle(req(
        "r-hold-other",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-2".into(),
            expected_store_id: store_id.clone(),
        },
    ));
    assert!(matches!(
        other_op.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));

    // Mismatched expected_store_id is rejected with StoreMismatch on ALL four commands.
    let wrong_store = "wrong-store-id".to_owned();
    for (req_id, request) in [
        (
            "r-bad-hold",
            RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move-op-1".into(),
                expected_store_id: wrong_store.clone(),
            },
        ),
        (
            "r-bad-discard",
            RelayRequest::DiscardHeldMoveCommands {
                operation_id: "move-op-1".into(),
                expected_store_id: wrong_store.clone(),
                command_ids: vec!["prompt-1".into()],
            },
        ),
        (
            "r-bad-front",
            RelayRequest::SubmitFront {
                operation_id: "move-op-1".into(),
                expected_store_id: wrong_store.clone(),
                command_id: "cmd-cont".into(),
                command: prompt_command("continuation"),
            },
        ),
        (
            "r-bad-release",
            RelayRequest::ReleaseInPlaceMoveDispatch {
                operation_id: "move-op-1".into(),
                expected_store_id: wrong_store,
            },
        ),
    ] {
        let resp = relay.handle(req(req_id, request));
        match resp.body {
            RelayResponseBody::Error { error } => {
                assert_eq!(error.code, RelayErrorCode::InvalidState);
                assert!(
                    matches!(error.detail, Some(RelayErrorDetail::StoreMismatch { .. })),
                    "expected StoreMismatch for {req_id}, got {error:?}"
                );
            }
            other => panic!("expected StoreMismatch error for {req_id}, got {other:?}"),
        }
    }

    // Idempotent discard: removing cmd-q1 twice succeeds without error.
    for req_id in ["r-discard-1", "r-discard-2"] {
        let resp = relay.handle(req(
            req_id,
            RelayRequest::DiscardHeldMoveCommands {
                operation_id: "move-op-1".into(),
                expected_store_id: store_id.clone(),
                command_ids: vec!["prompt-1".into()],
            },
        ));
        assert!(matches!(
            resp.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::HeldMoveCommandsDiscarded
            }
        ));
    }
    let remaining: Vec<&str> = relay
        .snapshot
        .queued_prompts
        .iter()
        .map(|q| q.command_id.as_str())
        .collect();
    assert_eq!(remaining, vec!["prompt-2", "prompt-3"]);
}

#[test]
fn submit_front_requires_hold_inserts_at_head_and_deduplicates() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

    // Rejected when dispatch is NOT held.
    let unheld = relay.handle(req(
        "r-front-unheld",
        RelayRequest::SubmitFront {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
            command_id: "cmd-cont".into(),
            command: prompt_command("resume work"),
        },
    ));
    assert!(matches!(
        unheld.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                ..
            }
        }
    ));

    // Hold dispatch, queue a normal prompt, then SubmitFront a continuation.
    let _ = relay.handle(req(
        "r-hold",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
        },
    ));
    let _ = relay.handle(req(
        "r-normal",
        RelayRequest::Submit {
            command_id: "cmd-normal".into(),
            command: prompt_command("normal queued prompt"),
        },
    ));
    let first = relay.handle(req(
        "r-front-1",
        RelayRequest::SubmitFront {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
            command_id: "cmd-cont".into(),
            command: prompt_command("resume work"),
        },
    ));
    let first_ordinal = match first.body {
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted { ordinal, .. },
        } => ordinal,
        other => panic!("expected Accepted, got {other:?}"),
    };

    // Repeat submission of the same command_id deduplicates without double-enqueueing.
    let second = relay.handle(req(
        "r-front-2",
        RelayRequest::SubmitFront {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id,
            command_id: "cmd-cont".into(),
            command: prompt_command("resume work"),
        },
    ));
    match second.body {
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted { ordinal, .. },
        } => assert_eq!(ordinal, first_ordinal),
        other => panic!("expected deduplicated Accepted, got {other:?}"),
    }

    let order: Vec<&str> = relay
        .snapshot
        .queued_prompts
        .iter()
        .map(|q| q.command_id.as_str())
        .collect();
    assert_eq!(order, vec!["cmd-cont", "cmd-normal"]);
    // Still held: nothing is claimable.
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
}

#[test]
fn in_place_move_hold_persists_across_reopen_and_releases_submit_front_first() {
    let temp = tempfile::tempdir().unwrap();
    let store_id = {
        let mut relay = configured(&temp);
        let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

        let _ = relay.handle(req(
            "r-hold",
            RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move-op-reopen".into(),
                expected_store_id: store_id.clone(),
            },
        ));
        let _ = relay.handle(req(
            "r-q1",
            RelayRequest::Submit {
                command_id: "cmd-queued".into(),
                command: prompt_command("queued behind continuation"),
            },
        ));
        let _ = relay.handle(req(
            "r-front",
            RelayRequest::SubmitFront {
                operation_id: "move-op-reopen".into(),
                expected_store_id: store_id.clone(),
                command_id: "cmd-continuation".into(),
                command: prompt_command("continuation first"),
            },
        ));
        assert!(relay.claim_pending_commands(true).unwrap().is_empty());
        store_id
    };

    // Reopen from relay-state.json: hold survives and nothing claims while held.
    let mut reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(reopened.snapshot.in_place_move_hold.is_some());
    assert!(reopened.claim_pending_commands(true).unwrap().is_empty());

    // Release the hold: SubmitFront continuation claims first ahead of cmd-queued.
    let released = reopened.handle(req(
        "r-release",
        RelayRequest::ReleaseInPlaceMoveDispatch {
            operation_id: "move-op-reopen".into(),
            expected_store_id: store_id.clone(),
        },
    ));
    assert!(matches!(
        released.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::InPlaceMoveDispatchReleased
        }
    ));
    // Releasing again is idempotent.
    let released_again = reopened.handle(req(
        "r-release-2",
        RelayRequest::ReleaseInPlaceMoveDispatch {
            operation_id: "move-op-reopen".into(),
            expected_store_id: store_id,
        },
    ));
    assert!(matches!(
        released_again.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::InPlaceMoveDispatchReleased
        }
    ));

    let claimed = reopened.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "cmd-continuation");
}

#[test]
fn relay_state_version_28_without_in_place_move_hold_decodes_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    {
        let _relay = configured(&temp);
    }
    let state_path = temp.path().join(RELAY_STATE_FILE);
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    saved["format_version"] = serde_json::json!(28);
    if let Some(obj) = saved.as_object_mut() {
        obj.remove("in_place_move_hold");
    }
    std::fs::write(&state_path, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();

    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(reopened.snapshot.format_version, RELAY_STATE_VERSION);
    assert!(reopened.snapshot.in_place_move_hold.is_none());
}

#[test]
fn in_place_harness_restart_requires_matching_hold_and_is_idempotent_after_mark() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

    // Rejected when no hold is active.
    let err = relay
        .admit_in_place_harness_restart("move-op-1", &store_id)
        .unwrap_err();
    assert_eq!(err.code, RelayErrorCode::InvalidState);

    // Take hold for move-op-1.
    let _ = relay.handle(req(
        "r-hold",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
        },
    ));

    // Rejected when operation_id mismatches.
    let err = relay
        .admit_in_place_harness_restart("move-op-other", &store_id)
        .unwrap_err();
    assert_eq!(err.code, RelayErrorCode::InvalidState);

    // Admitted for matching operation_id and store_id when idle.
    assert_eq!(
        relay
            .admit_in_place_harness_restart("move-op-1", &store_id)
            .unwrap(),
        InPlaceHarnessRestartDecision::Proceed
    );

    // Mark restarted durably; reopen verifies harness_restarted persists and
    // repeat admission returns AlreadyRestarted idempotently.
    relay
        .mark_in_place_harness_restarted("move-op-1", &store_id)
        .unwrap();
    let mut reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        reopened
            .admit_in_place_harness_restart("move-op-1", &store_id)
            .unwrap(),
        InPlaceHarnessRestartDecision::AlreadyRestarted
    );
}

#[test]
fn in_place_harness_restart_in_flight_blocks_release_and_mutations_until_completed() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

    let _ = relay.handle(req(
        "r-hold",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
        },
    ));
    let _ = relay.handle(req(
        "r-q1",
        RelayRequest::Submit {
            command_id: "prompt-queued".into(),
            command: prompt_command("queued prompt"),
        },
    ));

    // Admit harness restart -> in-flight reservation held.
    assert_eq!(
        relay
            .admit_in_place_harness_restart("move-op-1", &store_id)
            .unwrap(),
        InPlaceHarnessRestartDecision::Proceed
    );

    // Second concurrent restart admission is refused.
    let err = relay
        .admit_in_place_harness_restart("move-op-1", &store_id)
        .unwrap_err();
    assert_eq!(err.code, RelayErrorCode::InvalidState);

    assert!(
        matches!(err.detail, Some(RelayErrorDetail::ReloadInProgress { operation_id }) if operation_id == "move-op-1")
    );
    relay
        .mark_in_place_harness_committed("move-op-1", &store_id)
        .unwrap();
    let err = relay
        .admit_in_place_harness_restart("move-op-1", &store_id)
        .unwrap_err();
    assert!(
        matches!(err.detail, Some(RelayErrorDetail::ReloadInProgress { operation_id }) if operation_id == "move-op-1")
    );

    // Release, Discard, and SubmitFront are all refused while restart is in flight.
    for (req_id, request) in [
        (
            "r-rel-busy",
            RelayRequest::ReleaseInPlaceMoveDispatch {
                operation_id: "move-op-1".into(),
                expected_store_id: store_id.clone(),
            },
        ),
        (
            "r-disc-busy",
            RelayRequest::DiscardHeldMoveCommands {
                operation_id: "move-op-1".into(),
                expected_store_id: store_id.clone(),
                command_ids: Vec::new(),
            },
        ),
        (
            "r-front-busy",
            RelayRequest::SubmitFront {
                operation_id: "move-op-1".into(),
                expected_store_id: store_id.clone(),
                command_id: "prompt-cont".into(),
                command: prompt_command("continuation"),
            },
        ),
    ] {
        let resp = relay.handle(req(req_id, request));
        assert!(
            matches!(
                resp.body,
                RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::InvalidState,
                        ..
                    }
                }
            ),
            "expected InvalidState for {req_id}, got {:?}",
            resp.body
        );
    }
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());

    // Complete restart -> Release now succeeds and promotes queued prompt.
    relay
        .mark_in_place_harness_restarted("move-op-1", &store_id)
        .unwrap();
    let released = relay.handle(req(
        "r-rel-ok",
        RelayRequest::ReleaseInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id,
        },
    ));
    assert!(matches!(
        released.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::InPlaceMoveDispatchReleased
        }
    ));
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "prompt-queued");
}

#[test]
fn discard_held_move_commands_rejects_post_hold_prompts_without_mutating() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

    // Start an active turn so prompt-1 stays queued.
    let _ = relay.handle(req(
        "r-active",
        RelayRequest::Submit {
            command_id: "prompt-active".into(),
            command: prompt_command("active"),
        },
    ));
    assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
    let _ = relay.handle(req(
        "r-p1",
        RelayRequest::Submit {
            command_id: "prompt-1".into(),
            command: prompt_command("captured prompt"),
        },
    ));

    // Take hold -> captures only prompt-1.
    let _ = relay.handle(req(
        "r-hold",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
        },
    ));

    // A new prompt arrives after the hold was taken.
    let _ = relay.handle(req(
        "r-post",
        RelayRequest::Submit {
            command_id: "prompt-post-hold".into(),
            command: prompt_command("post-hold prompt"),
        },
    ));

    // Attempting to discard a batch containing the post-hold prompt is rejected
    // with InvalidRequest and mutates nothing.
    let bad_discard = relay.handle(req(
        "r-bad-discard",
        RelayRequest::DiscardHeldMoveCommands {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id.clone(),
            command_ids: vec!["prompt-1".into(), "prompt-post-hold".into()],
        },
    ));
    assert!(matches!(
        bad_discard.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidRequest,
                ..
            }
        }
    ));
    let queued_after_reject: Vec<&str> = relay
        .snapshot
        .queued_prompts
        .iter()
        .map(|q| q.command_id.as_str())
        .collect();
    assert_eq!(queued_after_reject, vec!["prompt-1", "prompt-post-hold"]);

    // Discarding only the captured prompt-1 succeeds and leaves prompt-post-hold intact.
    let ok_discard = relay.handle(req(
        "r-ok-discard",
        RelayRequest::DiscardHeldMoveCommands {
            operation_id: "move-op-1".into(),
            expected_store_id: store_id,
            command_ids: vec!["prompt-1".into()],
        },
    ));
    assert!(matches!(
        ok_discard.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::HeldMoveCommandsDiscarded
        }
    ));
    let queued_after_ok: Vec<&str> = relay
        .snapshot
        .queued_prompts
        .iter()
        .map(|q| q.command_id.as_str())
        .collect();
    assert_eq!(queued_after_ok, vec!["prompt-post-hold"]);
}

#[test]
fn in_place_move_hold_allows_active_turn_cancel_while_blocking_queued_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = configured(&temp);
    let store_id = relay.snapshot.store_id.clone().expect("store_id assigned");

    // Start an active prompt and queue a second prompt behind it.
    let _ = relay.handle(req(
        "r-active",
        RelayRequest::Submit {
            command_id: "prompt-active".into(),
            command: prompt_command("active turn"),
        },
    ));
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "prompt-active");

    let _ = relay.handle(req(
        "r-queued",
        RelayRequest::Submit {
            command_id: "prompt-queued".into(),
            command: prompt_command("queued behind active"),
        },
    ));

    // Take an in-place move hold while prompt-active is still running.
    let _ = relay.handle(req(
        "r-hold",
        RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: "move-op-cancel".into(),
            expected_store_id: store_id,
        },
    ));

    // Submit CancelTurn for the active turn: it must still be claimable during the hold,
    // while prompt-queued stays held.
    let cancel_resp = relay.handle(req(
        "r-cancel",
        RelayRequest::Submit {
            command_id: "cancel-active".into(),
            command: RelayCommand::CancelTurn,
        },
    ));
    assert!(matches!(cancel_resp.body, RelayResponseBody::Ok { .. }));

    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "cancel-active");
    assert!(matches!(claimed[0].command, RelayCommand::CancelTurn));

    // Even after the active turn finishes, prompt-queued must remain held.
    relay
        .record_command_completed(
            "prompt-active",
            RelayCommandOutcome::Prompt {
                stop_reason: "cancelled".into(),
                diagnostic: None,
                usage: None,
                work_facts: None,
            },
        )
        .unwrap();
    relay
        .record_command_completed("cancel-active", RelayCommandOutcome::Cancelled)
        .unwrap();
    assert!(relay.claim_pending_commands(true).unwrap().is_empty());
}

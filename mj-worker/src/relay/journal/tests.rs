use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, SessionUpdate};
use mj_core::relay::RESTORED_RELAY_SEED_FILE;
use serde_json::Value;

use super::*;
use crate::relay::test_support::*;
use crate::relay::{
    ClaimedRelayCommand, RELAY_EVENT_FORMAT_V1, RELAY_EVENT_GENESIS_DIGEST, RelayCommandKind,
    RelayCommandOutcome, RelayCursor, RelayErrorCode, RelayErrorDetail, RelayExecutionState,
    RelayProtocolError, RelayRequest, RelayResponseBody, RelayResponsePayload,
};

/// Streamed chunk cost, measured both ways in one process so a loaded
/// machine cannot flatter either policy. Run with
/// `cargo test --lib worker::journal::tests::streamed_chunk_append_cost
/// -- --ignored --nocapture`.
#[test]
#[ignore = "timing measurement, not a behavior assertion"]
fn streamed_chunk_append_cost() {
    const BACKLOG: usize = 40;
    const CHUNKS: usize = 200;
    const ROUNDS: u32 = 3;

    fn stream_chunks(stage_every_append: bool) -> (std::time::Duration, usize) {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        // Give the snapshot the weight a real session carries: a queue of
        // prompts the checkpoint has not pruned yet.
        for index in 0..BACKLOG {
            submit_relay(
                &mut relay,
                &format!("backlog-command-{index:04}"),
                prompt(&"q".repeat(4096)),
            );
        }
        let snapshot_bytes = fs::read(temp.path().join(RELAY_STATE_FILE)).unwrap().len();
        relay.stage_snapshot_every_append = stage_every_append;

        let chunk = "token ".repeat(40);
        let started = std::time::Instant::now();
        for _ in 0..CHUNKS {
            relay
                .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::from(chunk.clone()),
                )))
                .unwrap();
        }
        (started.elapsed(), snapshot_bytes)
    }

    let mut amortized = std::time::Duration::ZERO;
    let mut every_append = std::time::Duration::ZERO;
    let mut snapshot_bytes = 0;
    for _ in 0..ROUNDS {
        let (elapsed, bytes) = stream_chunks(false);
        amortized += elapsed;
        snapshot_bytes = bytes;
        every_append += stream_chunks(true).0;
    }

    // What one redundant snapshot write costs: the collection that follows
    // an advancing acknowledgement used to pay exactly this.
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    for index in 0..BACKLOG {
        submit_relay(
            &mut relay,
            &format!("backlog-command-{index:04}"),
            prompt(&"q".repeat(4096)),
        );
    }
    let started = std::time::Instant::now();
    for _ in 0..CHUNKS {
        relay.persist_snapshot().unwrap();
    }
    let persists = started.elapsed();

    let appends = CHUNKS as u32 * ROUNDS;
    println!(
        "snapshot {snapshot_bytes} bytes, {appends} chunk appends per policy\n  \
             snapshot per append: {every_append:?} ({:?}/append)\n  \
             amortized:           {amortized:?} ({:?}/append)\n  \
             one snapshot write:  {:?}",
        every_append / appends,
        amortized / appends,
        persists / u32::try_from(CHUNKS).unwrap(),
    );
}

fn persisted_relay_snapshot(root: &Path) -> RelaySnapshot {
    serde_json::from_slice(&fs::read(root.join(RELAY_STATE_FILE)).unwrap()).unwrap()
}

/// The journal is what makes a streamed chunk durable. Rewriting the whole
/// snapshot per chunk bought nothing, because recovery already replays
/// journal events past the snapshot frontier.
#[test]
fn streamed_chunks_are_journaled_without_rewriting_the_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "streaming-prompt", prompt("stream"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "streaming-prompt"
    );
    let persisted = persisted_relay_snapshot(temp.path());

    for index in 0..8 {
        relay
            .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from(format!("chunk {index}")),
            )))
            .unwrap();
    }
    assert_eq!(relay.latest_ordinal(), persisted.latest_ordinal + 8);
    assert_eq!(
        persisted_relay_snapshot(temp.path()),
        persisted,
        "streamed chunks rewrote relay-state.json"
    );

    // A state move is still durable the moment it is recorded.
    finish_prompt(&mut relay, "streaming-prompt");
    assert_eq!(
        persisted_relay_snapshot(temp.path()).latest_ordinal,
        relay.latest_ordinal()
    );
}

/// A relay that dies mid-turn keeps every chunk it acknowledged, and the
/// relay that reopens republishes the frontier it replayed.
#[test]
fn a_relay_that_dies_mid_stream_recovers_its_unpersisted_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    // The prompt stays promoted but unclaimed, so recovery has no in-flight
    // ACP command to interrupt and the frontier is exactly what was
    // journaled.
    submit_relay(&mut relay, "interrupted-prompt", prompt("stream"));
    for index in 0..8 {
        relay
            .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from(format!("chunk {index}")),
            )))
            .unwrap();
    }
    let frontier = relay.latest_ordinal();
    let digest = relay.latest_digest().to_owned();
    // No teardown: the process is gone before anything else is written.
    drop(relay);

    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(reopened.latest_ordinal(), frontier);
    assert_eq!(reopened.latest_digest(), digest);
    assert_eq!(
        reopened
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .len(),
        usize::try_from(frontier).unwrap()
    );
    assert_eq!(
        persisted_relay_snapshot(temp.path()).latest_ordinal,
        frontier,
        "recovery must republish the frontier it replayed"
    );
}

/// Amortization is bounded: a long stream still rewrites the snapshot, so
/// a restart never has to replay an unbounded journal.
#[test]
fn a_long_stream_persists_the_snapshot_before_replay_grows_unbounded() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let chunk = "y".repeat(64 * 1024);
    let mut journaled = 0_usize;
    while journaled <= RELAY_SNAPSHOT_LAG_BYTE_LIMIT {
        relay
            .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::from(chunk.clone()),
            )))
            .unwrap();
        journaled += chunk.len();
    }

    let persisted = persisted_relay_snapshot(temp.path()).latest_ordinal;
    assert!(
        persisted > 1,
        "a stream past the replay budget never rewrote the snapshot"
    );
    assert!(persisted <= relay.latest_ordinal());
}

/// A catch-up acknowledgement wrote `relay-state.json` twice: once for the
/// ACK, then again for a collection that had nothing to collect.
#[test]
fn an_acknowledgement_with_nothing_to_collect_does_not_rewrite_the_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    // Pin retained history at a verified checkpoint and let the collection
    // that follows it finish.
    let ready = ready_checkpoint(&mut relay, "catch-up-barrier");
    acknowledge_relay(&mut relay, "ack-catch-up-barrier", ready.ordinal);
    submit_relay(
        &mut relay,
        "complete-catch-up",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "catch-up-barrier".into(),
        },
    );
    relay
        .record_observation(RelayObservation::Warning {
            message: "streamed past the floor".into(),
        })
        .unwrap();
    let through = relay.latest_ordinal();
    acknowledge_relay(&mut relay, "ack-catch-up", through);

    // From here any snapshot write fails, so a redundant one is visible.
    let state_path = temp.path().join(RELAY_STATE_FILE);
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();
    let repeated = acknowledge_relay(&mut relay, "ack-catch-up-again", through);
    assert!(
        matches!(
            repeated.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Acknowledged {
                    through_ordinal,
                    ..
                }
            } if through_ordinal == through
        ),
        "history collection rewrote an unchanged snapshot: {:?}",
        repeated.body
    );
}

#[test]
fn restart_finishes_a_durably_accepted_queue_removal() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "active-prompt", prompt("active"));
    submit_relay(&mut relay, "queued-prompt", prompt("remove me"));
    relay
        .append_relay_event(
            Some("remove-after-crash"),
            RelayObservation::CommandQueued {
                command_id: "remove-after-crash".into(),
                command: RelayCommand::RemoveQueuedPrompt {
                    queued_command_id: "queued-prompt".into(),
                },
                created_at_ms: epoch_millis(),
            },
        )
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(relay.operational_state().queued_prompts.is_empty());
    assert!(retained_events(&relay).iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandCompleted {
            command_id,
            outcome: RelayCommandOutcome::QueueChanged { removed_command_ids },
        } if command_id == "remove-after-crash"
            && removed_command_ids == &["queued-prompt".to_owned()]
    )));
}

#[test]
fn restart_finishes_a_durably_accepted_queue_clear() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "active-prompt", prompt("active"));
    submit_relay(&mut relay, "queued-one", prompt("one"));
    submit_relay(&mut relay, "queued-two", prompt("two"));
    relay
        .append_relay_event(
            Some("clear-after-crash"),
            RelayObservation::CommandQueued {
                command_id: "clear-after-crash".into(),
                command: RelayCommand::ClearQueuedPrompts,
                created_at_ms: epoch_millis(),
            },
        )
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(relay.operational_state().queued_prompts.is_empty());
    assert!(retained_events(&relay).iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandCompleted {
            command_id,
            outcome: RelayCommandOutcome::QueueChanged { removed_command_ids },
        } if command_id == "clear-after-crash"
            && removed_command_ids
                == &["queued-one".to_owned(), "queued-two".to_owned()]
    )));
}

#[test]
fn restart_finishes_checkpoint_completion_before_releasing_ownerless_barriers() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let ready = ready_checkpoint(&mut relay, "barrier-command");
    relay
        .append_relay_event(
            Some("complete-after-crash"),
            RelayObservation::CommandQueued {
                command_id: "complete-after-crash".into(),
                command: RelayCommand::CompleteCheckpoint {
                    barrier_command_id: "barrier-command".into(),
                },
                created_at_ms: epoch_millis(),
            },
        )
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.snapshot.recovery_floor_ordinal, ready.ordinal);
    assert!(relay.snapshot.checkpoint_barrier.is_none());
    assert!(!retained_events(&relay).iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandInterrupted { command_id, .. }
            if command_id == "barrier-command"
    )));
}

#[test]
fn restart_interrupts_ownerless_checkpoint_but_preserves_accepted_close() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let expected = ready_checkpoint(&mut relay, "close-barrier");
    submit_relay(
        &mut relay,
        "accepted-close",
        RelayCommand::Close {
            barrier_command_id: "close-barrier".into(),
            expected,
        },
    );
    drop(relay);

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert!(retained_events(&relay).iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandInterrupted {
            command_id,
            command: RelayCommandKind::BeginCheckpoint,
            ..
        } if command_id == "close-barrier"
    )));
    assert_eq!(
        relay.operational_state().execution,
        RelayExecutionState::Closing
    );
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert!(matches!(
        claimed.as_slice(),
        [ClaimedRelayCommand {
            command_id,
            command: RelayCommand::Close { .. },
            ..
        }] if command_id == "accepted-close"
    ));
}

#[test]
fn relay_command_submission_is_idempotent_across_restart() {
    let temp = tempfile::tempdir().unwrap();
    let first_ordinal;
    {
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        first_ordinal = submit_relay(&mut relay, "stable-command", prompt("once"));
    }
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let repeated = submit_relay(&mut relay, "stable-command", prompt("once"));
    assert_eq!(repeated, first_ordinal);
    assert_eq!(
        retained_events(&relay)
            .iter()
            .filter(|event| matches!(
                &event.observation,
                RelayObservation::CommandQueued { command_id, .. }
                    if command_id == "stable-command"
            ))
            .count(),
        1
    );

    let response = relay.handle(relay_request(
        "request-conflict",
        RelayRequest::Submit {
            command_id: "stable-command".into(),
            command: prompt("different"),
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
}

#[test]
fn command_idempotency_survives_ack_until_checkpoint_covers_terminal_event() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let accepted = submit_relay(&mut relay, "checkpointed-command", prompt("once"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "checkpointed-command"
    );
    relay
        .record_command_completed(
            "checkpointed-command",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    let terminal = relay.latest_ordinal();
    attach_relay(&mut relay, "attach-idempotency", 0);
    acknowledge_relay(&mut relay, "ack-idempotency", terminal);
    assert_eq!(
        submit_relay(&mut relay, "checkpointed-command", prompt("once")),
        accepted,
        "ACK must not prune the stable command ID"
    );

    let ready = ready_checkpoint(&mut relay, "idempotency-barrier");
    acknowledge_relay(&mut relay, "ack-idempotency-barrier", ready.ordinal);
    submit_relay(
        &mut relay,
        "complete-idempotency-barrier",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "idempotency-barrier".into(),
        },
    );
    let accepted_again = submit_relay(&mut relay, "checkpointed-command", prompt("once"));
    assert!(accepted_again > accepted);
}

#[test]
fn restart_redispatches_a_promoted_configuration_change() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "config-command", set_config("model", "sonnet"));
    // The change was started but never handed to ACP.
    assert!(queued_command_ids(&relay).is_empty());
    drop(relay);

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "config-command");
    assert!(matches!(claimed[0].command, RelayCommand::SetConfig { .. }));
}

#[test]
fn restart_adopts_a_config_accepted_outside_the_durable_queue() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    // The prompt is started but never handed to ACP, so restarting keeps
    // it active instead of interrupting it.
    submit_relay(&mut relay, "active-prompt", prompt("running"));
    submit_relay(&mut relay, "config-queued", set_config("model", "sonnet"));
    drop(relay);

    // Rewrite the durable snapshot the way an older relay wrote it: the
    // configuration change is accepted, but not in the command queue.
    let state_path = temp.path().join(RELAY_STATE_FILE);
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["queued_prompts"]
        .as_array_mut()
        .unwrap()
        .retain(|queued| queued["command_id"] != "config-queued");
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(queued_command_ids(&relay), ["config-queued"]);
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "active-prompt"
    );
    finish_prompt(&mut relay, "active-prompt");
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "config-queued"
    );
}

#[test]
fn acknowledgement_only_garbage_collects_through_a_verified_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "remember me".into(),
        })
        .unwrap();
    let attach = attach_relay(&mut relay, "attach-1", 0);
    assert!(matches!(
        attach.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Attached {
                ref events,
                through_ordinal: 1,
                ..
            }
        } if events.len() == 1
    ));
    let acknowledged = acknowledge_relay(&mut relay, "ack-1", 1);
    assert!(matches!(
        acknowledged.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Acknowledged {
                through_ordinal: 1,
                ..
            }
        }
    ));
    assert_eq!(
        retained_events(&relay).len(),
        1,
        "ACK alone is not a recovery cut"
    );

    let ready = ready_checkpoint(&mut relay, "gc-barrier");
    acknowledge_relay(&mut relay, "ack-checkpoint", ready.ordinal);
    submit_relay(
        &mut relay,
        "complete-checkpoint",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "gc-barrier".into(),
        },
    );
    assert!(
        retained_events(&relay)
            .iter()
            .all(|event| event.ordinal > ready.ordinal)
    );

    drop(relay);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let stale = attach_relay(&mut relay, "attach-stale", 0);
    assert!(matches!(
        stale.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Desynchronized,
                detail: Some(RelayErrorDetail::Desynchronized {
                    earliest_available,
                    ..
                }),
                ..
            }
        } if earliest_available == ready.ordinal
    ));
}

#[test]
fn relay_recovers_an_event_fsynced_before_its_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    drop(relay);
    let event = RelayEvent {
        format: RELAY_EVENT_FORMAT_V1,
        ordinal: 1,
        previous_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        digest: String::new(),
        recorded_at_ms: epoch_millis(),
        command_id: None,
        observation: RelayObservation::Warning {
            message: "after journal fsync".into(),
        },
    };
    let event = RelayEvent {
        digest: relay_event_digest(&event).unwrap(),
        ..event
    };
    let path = temp
        .path()
        .join(RELAY_JOURNAL_DIR)
        .join(RELAY_ACTIVE_SEGMENT);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    serde_json::to_writer(&mut file, &event).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), 1);
    assert_eq!(
        relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap(),
        vec![event]
    );
}

#[test]
fn relay_truncates_a_torn_active_tail_before_appending_again() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "durable prefix".into(),
        })
        .unwrap();
    let active = temp
        .path()
        .join(RELAY_JOURNAL_DIR)
        .join(RELAY_ACTIVE_SEGMENT);
    let durable_len = active.metadata().unwrap().len();
    drop(relay);

    let mut file = OpenOptions::new().append(true).open(&active).unwrap();
    file.write_all(br#"{"ordinal":2,"previous_digest":"#)
        .unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(active.metadata().unwrap().len() > durable_len);

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), 1);
    assert_eq!(active.metadata().unwrap().len(), durable_len);
    relay
        .record_observation(RelayObservation::Warning {
            message: "after repair".into(),
        })
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), 2);
    assert_eq!(retained_events(&relay).len(), 2);
}

#[test]
fn a_read_only_replay_tolerates_a_torn_active_tail_and_serves_the_newest_record() {
    // An attach serving the hot segment can read it while the worker is
    // mid-append, catching a final record with no trailing newline. That
    // partial write must not fail the replay: every complete record —
    // including the most recent — has to be delivered, and the live file
    // must be left untouched for the writer to finish.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(RELAY_ACTIVE_SEGMENT);
    let mut file = File::create(&path).unwrap();
    let mut previous_digest = RELAY_EVENT_GENESIS_DIGEST.to_owned();
    for ordinal in 1..=3 {
        let event = RelayEvent {
            format: RELAY_EVENT_FORMAT_V1,
            ordinal,
            previous_digest: previous_digest.clone(),
            digest: String::new(),
            recorded_at_ms: epoch_millis(),
            command_id: None,
            observation: RelayObservation::Warning {
                message: format!("event {ordinal}"),
            },
        };
        let event = RelayEvent {
            digest: relay_event_digest(&event).unwrap(),
            ..event
        };
        previous_digest = event.digest.clone();
        serde_json::to_writer(&mut file, &event).unwrap();
        file.write_all(b"\n").unwrap();
    }
    // A torn, in-flight fourth record: content began but the closing bytes
    // and newline are not on disk yet.
    file.write_all(br#"{"ordinal":4,"previous_digest":"#)
        .unwrap();
    file.sync_all().unwrap();
    let len_before = path.metadata().unwrap().len();

    let mut seen = Vec::new();
    visit_relay_journal_file(&path, JournalReadMode::Strict, |event, _| {
        seen.push(event.ordinal);
        Ok(ControlFlow::Continue(()))
    })
    .expect("a read-only replay must tolerate a torn tail");

    assert_eq!(
        seen,
        vec![1, 2, 3],
        "every complete record, including the most recent, must be served"
    );
    assert_eq!(
        path.metadata().unwrap().len(),
        len_before,
        "a read-only replay must not truncate the live segment"
    );
}

#[test]
fn recover_mode_skips_a_corrupt_record_and_recovers_its_neighbours() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(RELAY_ACTIVE_SEGMENT);
    fn write_event(file: &mut File, ordinal: u64, previous_digest: &mut String) {
        let event = RelayEvent {
            format: RELAY_EVENT_FORMAT_V1,
            ordinal,
            previous_digest: previous_digest.clone(),
            digest: String::new(),
            recorded_at_ms: ordinal as i64,
            command_id: None,
            observation: RelayObservation::Warning {
                message: format!("event {ordinal}"),
            },
        };
        let event = RelayEvent {
            digest: relay_event_digest(&event).unwrap(),
            ..event
        };
        *previous_digest = event.digest.clone();
        serde_json::to_writer(&mut *file, &event).unwrap();
        file.write_all(b"\n").unwrap();
    }

    let mut file = File::create(&path).unwrap();
    let mut previous_digest = RELAY_EVENT_GENESIS_DIGEST.to_owned();
    write_event(&mut file, 1, &mut previous_digest);
    // A corrupt interior record: terminated (has its newline) but not valid
    // JSON, so its bytes are unrecoverable.
    file.write_all(br#"{"ordinal":2,"observation": BROKEN"#)
        .unwrap();
    file.write_all(b"\n").unwrap();
    write_event(&mut file, 3, &mut previous_digest);
    file.sync_all().unwrap();

    // Strict aborts on the corrupt record.
    let strict = visit_relay_journal_file(&path, JournalReadMode::Strict, |_, _| {
        Ok(ControlFlow::Continue(()))
    });
    assert!(
        strict.is_err(),
        "strict reads must not silently pass a corrupt record"
    );

    // Recover skips it and delivers both intact neighbours, reporting one gap.
    let mut seen = Vec::new();
    let gaps = visit_relay_journal_file(&path, JournalReadMode::Recover, |event, _| {
        seen.push(event.ordinal);
        Ok(ControlFlow::Continue(()))
    })
    .expect("recovery must not fail on a corrupt record");
    assert_eq!(
        seen,
        vec![1, 3],
        "records either side of the corruption must be recovered"
    );
    assert_eq!(gaps.len(), 1, "the one corrupt record is reported as a gap");
}

#[test]
fn recovery_isolates_a_corrupt_record_at_every_position() {
    // "Recover everything that isn't actively corrupted": whichever single
    // record is corrupt, all the others come back and exactly one gap is
    // reported.
    const COUNT: u64 = 6;
    for corrupt_index in 1..=COUNT {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(RELAY_ACTIVE_SEGMENT);
        let mut file = File::create(&path).unwrap();
        let mut previous_digest = RELAY_EVENT_GENESIS_DIGEST.to_owned();
        for ordinal in 1..=COUNT {
            if ordinal == corrupt_index {
                file.write_all(br#"{"ordinal":0,"observation": BROKEN"#)
                    .unwrap();
                file.write_all(b"\n").unwrap();
                continue;
            }
            let event = RelayEvent {
                format: RELAY_EVENT_FORMAT_V2,
                ordinal,
                previous_digest: String::new(),
                digest: String::new(),
                recorded_at_ms: ordinal as i64,
                command_id: None,
                observation: RelayObservation::Warning {
                    message: format!("event {ordinal}"),
                },
            };
            let event = RelayEvent {
                digest: relay_event_digest(&event).unwrap(),
                ..event
            };
            // v2 records self-hash; keep a running frontier only to mimic
            // the writer, though v2 does not fold it in.
            previous_digest = event.digest.clone();
            serde_json::to_writer(&mut file, &event).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.sync_all().unwrap();
        let _ = &previous_digest;

        let mut seen = Vec::new();
        let gaps = visit_relay_journal_file(&path, JournalReadMode::Recover, |event, _| {
            validate_relay_event_self(&event).unwrap();
            seen.push(event.ordinal);
            Ok(ControlFlow::Continue(()))
        })
        .expect("recovery must not fail on a corrupt record");
        let expected: Vec<u64> = (1..=COUNT).filter(|o| *o != corrupt_index).collect();
        assert_eq!(
            seen, expected,
            "every intact record must survive corruption at index {corrupt_index}"
        );
        assert_eq!(
            gaps.len(),
            1,
            "one gap for corruption at index {corrupt_index}"
        );
    }
}

#[test]
fn a_journal_mixing_v1_and_v2_records_reads_and_validates_across_the_boundary() {
    // During the lazy migration a journal holds legacy v1 records followed
    // by new v2 records. Both formats read, each self-validates, and the
    // v1→v2 boundary validates without a chain link.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(RELAY_ACTIVE_SEGMENT);
    let mut file = File::create(&path).unwrap();

    let v1 = RelayEvent {
        format: RELAY_EVENT_FORMAT_V1,
        ordinal: 1,
        previous_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
        digest: String::new(),
        recorded_at_ms: 1,
        command_id: None,
        observation: RelayObservation::Warning {
            message: "legacy".into(),
        },
    };
    let v1 = RelayEvent {
        digest: relay_event_digest(&v1).unwrap(),
        ..v1
    };
    serde_json::to_writer(&mut file, &v1).unwrap();
    file.write_all(b"\n").unwrap();

    let v2 = RelayEvent {
        format: RELAY_EVENT_FORMAT_V2,
        ordinal: 2,
        previous_digest: String::new(),
        digest: String::new(),
        recorded_at_ms: 2,
        command_id: None,
        observation: RelayObservation::Warning {
            message: "new".into(),
        },
    };
    let v2 = RelayEvent {
        digest: relay_event_digest(&v2).unwrap(),
        ..v2
    };
    serde_json::to_writer(&mut file, &v2).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();

    let mut events = Vec::new();
    visit_relay_journal_file(&path, JournalReadMode::Strict, |event, _| {
        events.push(event);
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].format, RELAY_EVENT_FORMAT_V1);
    assert_eq!(events[1].format, RELAY_EVENT_FORMAT_V2);
    // The v2 record continues from the v1 record across the format
    // boundary: ordinal contiguity holds and the v2 record self-validates,
    // with no chain link required.
    validate_relay_event(v1.ordinal, &v1.digest, &events[1]).unwrap();
}

#[test]
fn first_active_journal_file_is_reopenable_after_its_first_append() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "first durable event".into(),
        })
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), 1);
    assert_eq!(retained_events(&relay).len(), 1);
}

#[test]
fn failed_gc_persistence_keeps_command_idempotency_in_memory() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let accepted = submit_relay(&mut relay, "keep-idempotent", prompt("once"));
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "keep-idempotent"
    );
    relay
        .record_command_completed(
            "keep-idempotent",
            RelayCommandOutcome::Prompt {
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
    let terminal = relay.latest_ordinal();
    let digest = relay.latest_digest().to_owned();
    relay.snapshot.acknowledged_through = terminal;
    relay.snapshot.acknowledged_digest.clone_from(&digest);
    relay.snapshot.recovery_floor_ordinal = terminal;
    relay.snapshot.recovery_floor_digest = digest;
    relay.persist_snapshot().unwrap();

    let state_path = temp.path().join(RELAY_STATE_FILE);
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();
    let error = relay.garbage_collect_relay_history().unwrap_err();
    assert!(format!("{error:#}").contains("relay-state.json"));
    assert!(
        relay
            .snapshot
            .handled_commands
            .contains_key("keep-idempotent")
    );
    assert_eq!(
        submit_relay(&mut relay, "keep-idempotent", prompt("once")),
        accepted
    );
}

#[test]
fn retry_resumes_a_relay_local_command_after_snapshot_persistence_failure() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let state_path = temp.path().join(RELAY_STATE_FILE);
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();
    let command = RelayCommand::ClearQueuedPrompts;

    let failed = relay.handle(relay_request(
        "first-local-attempt",
        RelayRequest::Submit {
            command_id: "retry-local".into(),
            command: command.clone(),
        },
    ));
    assert!(matches!(
        failed.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                ..
            }
        }
    ));
    assert_eq!(
        relay.snapshot.dispatches["retry-local"].state,
        RelayDispatchState::Queued
    );

    fs::remove_dir(&state_path).unwrap();
    let retried = relay.handle(relay_request(
        "retry-local-attempt",
        RelayRequest::Submit {
            command_id: "retry-local".into(),
            command,
        },
    ));
    assert!(matches!(
        retried.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted { .. }
        }
    ));
    assert_eq!(
        relay.snapshot.dispatches["retry-local"].state,
        RelayDispatchState::Completed
    );
}

#[test]
fn duplicate_acknowledgement_retries_incomplete_journal_gc() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "collect after retry".into(),
        })
        .unwrap();
    let through = relay.latest_ordinal();
    let digest = relay.latest_digest().to_owned();
    relay.snapshot.acknowledged_through = through;
    relay.snapshot.acknowledged_digest.clone_from(&digest);
    relay.snapshot.recovery_floor_ordinal = through;
    relay.snapshot.recovery_floor_digest.clone_from(&digest);
    // The snapshot stays unpersisted, so the collection below has to make
    // the retained frontier durable before it drops history under it.

    let state_path = temp.path().join(RELAY_STATE_FILE);
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();
    let failed = relay.handle(relay_request(
        "gc-fails-after-ack",
        RelayRequest::Acknowledge {
            through_ordinal: through,
            through_digest: digest.clone(),
        },
    ));
    assert!(matches!(
        failed.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                ..
            }
        }
    ));

    fs::remove_dir(&state_path).unwrap();
    let retried = relay.handle(relay_request(
        "gc-retry-after-ack",
        RelayRequest::Acknowledge {
            through_ordinal: through,
            through_digest: digest.clone(),
        },
    ));
    assert!(matches!(
        retried.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Acknowledged {
                through_ordinal,
                ..
            }
        } if through_ordinal == through
    ));
    assert_eq!(relay.retained_event_body_count(), 0);
    assert!(relay.events_after(through, &digest).unwrap().is_empty());
}

#[test]
fn failed_ack_persistence_does_not_advance_the_live_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "retain until ACK is durable".into(),
        })
        .unwrap();
    let digest = relay.latest_digest().to_owned();
    let state_path = temp.path().join(RELAY_STATE_FILE);
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();

    let failed = relay.handle(relay_request(
        "ack-persistence-fails",
        RelayRequest::Acknowledge {
            through_ordinal: 1,
            through_digest: digest.clone(),
        },
    ));
    assert!(matches!(
        failed.body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                ..
            }
        }
    ));
    assert_eq!(relay.acknowledged_through(), 0);
    assert_eq!(retained_events(&relay).len(), 1);

    fs::remove_dir(&state_path).unwrap();
    let retry = relay.handle(relay_request(
        "ack-persistence-retry",
        RelayRequest::Acknowledge {
            through_ordinal: 1,
            through_digest: digest,
        },
    ));
    assert!(matches!(
        retry.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Acknowledged {
                through_ordinal: 1,
                ..
            }
        }
    ));
}

#[test]
fn failed_claim_persistence_leaves_the_command_claimable() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    submit_relay(&mut relay, "claim-retry", prompt("run once"));
    assert_eq!(
        relay.snapshot.dispatches["claim-retry"].state,
        RelayDispatchState::Pending
    );
    let state_path = temp.path().join(RELAY_STATE_FILE);
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();

    assert!(relay.claim_pending_commands(true).is_err());
    assert_eq!(
        relay.snapshot.dispatches["claim-retry"].state,
        RelayDispatchState::Pending
    );

    fs::remove_dir(&state_path).unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "claim-retry");
    assert_eq!(
        relay.snapshot.dispatches["claim-retry"].state,
        RelayDispatchState::InFlight
    );
}

/// A relay that truncated an event must still be able to reopen the
/// journal it wrote — the readback path bounds lines by the same budget.
#[test]
fn a_truncated_event_can_be_read_back_after_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::from("y".repeat(3 * 1024 * 1024)),
        )))
        .unwrap();
    drop(relay);

    let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(reopened.latest_ordinal(), 1);
}

#[test]
fn sealed_relay_segments_replay_and_are_removed_after_checkpointed_ack() {
    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "sealed".into(),
        })
        .unwrap();
    let mut metadata = relay
        .journal_spans
        .iter()
        .find(|span| span.path.ends_with(RELAY_ACTIVE_SEGMENT))
        .unwrap()
        .clone();
    seal_active_relay_segment(&temp.path().join(RELAY_JOURNAL_DIR), &mut metadata).unwrap();
    assert!(
        fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "gz"))
    );
    drop(relay);

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        relay
            .events_after(0, RELAY_EVENT_GENESIS_DIGEST)
            .unwrap()
            .len(),
        1
    );
    let _ = attach_relay(&mut relay, "attach-sealed", 0);
    let _ = acknowledge_relay(&mut relay, "ack-sealed", 1);
    assert!(
        fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "gz"))
    );
    let ready = ready_checkpoint(&mut relay, "sealed-barrier");
    acknowledge_relay(&mut relay, "ack-sealed-checkpoint", ready.ordinal);
    submit_relay(
        &mut relay,
        "sealed-complete",
        RelayCommand::CompleteCheckpoint {
            barrier_command_id: "sealed-barrier".into(),
        },
    );
    assert!(
        !fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "gz"))
    );
}

#[test]
fn reopening_preserves_a_stale_active_copy_before_appending() {
    let temp = tempfile::tempdir().unwrap();
    let journal = temp.path().join(RELAY_JOURNAL_DIR);
    let active = journal.join(RELAY_ACTIVE_SEGMENT);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    for message in ["first", "second"] {
        relay
            .record_observation(RelayObservation::Warning {
                message: message.into(),
            })
            .unwrap();
    }
    relay.persist_snapshot().unwrap();
    drop(relay);

    let active_bytes = fs::read(&active).unwrap();
    let first_line_end = active_bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let stale_bytes = active_bytes[..first_line_end].to_vec();
    let mut metadata = inspect_relay_journal_file(&active, false).unwrap().unwrap();
    seal_active_relay_segment(&journal, &mut metadata).unwrap();
    fs::write(&active, &stale_bytes).unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), 2);
    assert!(fs::read(&active).unwrap().is_empty());
    let archived = fs::read_dir(&journal)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("stale-active-"))
        })
        .expect("stale active journal was not preserved");
    assert_eq!(fs::read(archived).unwrap(), stale_bytes);

    relay
        .record_observation(RelayObservation::Warning {
            message: "third".into(),
        })
        .unwrap();
    assert_eq!(relay.latest_ordinal(), 3);
}

#[test]
fn replay_lazily_resolves_an_old_sealed_segment_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let journal = temp.path().join(RELAY_JOURNAL_DIR);
    let active = journal.join(RELAY_ACTIVE_SEGMENT);
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let mut first_digest = None;
    for index in 0..=RELAY_HOT_EVENT_CAPACITY {
        relay
            .record_observation(RelayObservation::Warning {
                message: format!("event {index}"),
            })
            .unwrap();
        if index == 0 {
            first_digest = Some(relay.latest_digest().to_owned());
        }
        let span = relay
            .journal_spans
            .iter_mut()
            .find(|span| span.path == active)
            .unwrap();
        seal_active_relay_segment(&journal, span).unwrap();
    }
    relay.persist_snapshot().unwrap();
    drop(relay);

    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let events = relay
        .events_after(1, &first_digest.unwrap())
        .expect("old segment boundary should resolve from the next sealed segment");
    assert_eq!(events.first().unwrap().ordinal, 2);
}

#[test]
fn large_relay_history_stays_disk_backed_and_replays_across_segments() {
    const EVENT_COUNT: usize = 80;
    const MESSAGE_BYTES: usize = 64 * 1024;

    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    for index in 0..EVENT_COUNT {
        relay
            .record_observation(RelayObservation::Warning {
                message: format!("{index:04}:{}", "x".repeat(MESSAGE_BYTES)),
            })
            .unwrap();
    }
    assert_eq!(relay.retained_event_body_count(), RELAY_HOT_EVENT_CAPACITY);
    assert!(
        fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "gz"))
            .count()
            >= 2,
        "test history did not cross multiple sealed journal files"
    );
    let expected_latest = relay.latest_ordinal();
    let expected_digest = relay.latest_digest().to_owned();
    drop(relay);

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), expected_latest);
    assert_eq!(relay.latest_digest(), expected_digest);
    assert_eq!(relay.retained_event_body_count(), RELAY_HOT_EVENT_CAPACITY);

    let mut cursor = RelayCursor {
        ordinal: 0,
        digest: RELAY_EVENT_GENESIS_DIGEST.into(),
    };
    let mut replayed = 0_usize;
    let mut pages = 0_usize;
    while cursor.ordinal < expected_latest {
        let response = relay.handle(relay_request(
            &format!("paged-replay-{pages}"),
            RelayRequest::Attach {
                after_ordinal: cursor.ordinal,
                after_digest: cursor.digest.clone(),
            },
        ));
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::Attached {
                    events,
                    through_ordinal,
                    through_digest,
                    ..
                },
        } = response.body
        else {
            panic!("disk-backed replay failed: {:?}", response.body);
        };
        assert!(!events.is_empty());
        for event in &events {
            validate_relay_event(cursor.ordinal, &cursor.digest, event).unwrap();
            cursor.ordinal = event.ordinal;
            cursor.digest = event.digest.clone();
        }
        assert_eq!(cursor.ordinal, through_ordinal);
        assert_eq!(cursor.digest, through_digest);
        replayed += events.len();
        pages += 1;
    }
    assert_eq!(replayed, EVENT_COUNT);
    assert!(pages >= 2, "history unexpectedly fit in one replay page");
    assert_eq!(cursor.digest, expected_digest);
}

#[test]
fn reopening_does_not_decompress_historical_sealed_segments() {
    const EVENT_COUNT: usize = 80;
    const MESSAGE_BYTES: usize = 64 * 1024;

    let temp = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    for index in 0..EVENT_COUNT {
        relay
            .record_observation(RelayObservation::Warning {
                message: format!("{index:04}:{}", "x".repeat(MESSAGE_BYTES)),
            })
            .unwrap();
    }
    drop(relay);

    let mut sealed = fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "gz"))
        .collect::<Vec<_>>();
    sealed.sort();
    assert!(
        sealed.len() >= 3,
        "test history did not seal enough segments"
    );
    fs::write(&sealed[0], b"not a gzip stream").unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0")
        .expect("current snapshot should open without reading old transcript segments");
    let response = relay.handle(relay_request(
        "attach-corrupt-history",
        RelayRequest::Attach {
            after_ordinal: 0,
            after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
        },
    ));
    let RelayResponseBody::Error { error } = response.body else {
        panic!("corrupt history was served: {response:?}");
    };
    // Corrupt history is never served as valid data. Because readable
    // history exists past the corrupt segment, the relay answers with a
    // recovery cursor so the controller resynchronizes forward instead of
    // retrying the unreadable bytes forever.
    assert_eq!(error.code, RelayErrorCode::Desynchronized);
    let Some(RelayErrorDetail::Desynchronized {
        earliest_available,
        latest,
        ..
    }) = error.detail
    else {
        panic!("expected a desync recovery cursor: {error:?}");
    };
    assert!(
        earliest_available > 0,
        "recovery cursor must skip the corrupt segment, got {earliest_available}"
    );
    assert!(
        earliest_available < latest,
        "newer readable history must remain available: {earliest_available} < {latest}"
    );
}

#[test]
fn restored_relay_continues_after_canonical_event_frontier() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
            restored_relay_seed_path(temp.path()),
            serde_json::to_vec(&serde_json::json!({
                "event_frontier": 41,
                "event_frontier_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "queued_prompts": [{
                    "command_id": "restored-command",
                    "content": [{"type": "text", "text": "continue offline"}],
                    "queued_at_ms": 1234
                }]
            }))
            .unwrap(),
        )
        .unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(relay.latest_ordinal(), 42);
    assert_eq!(relay.acknowledged_through(), 41);
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "restored-command"
    );
    let ordinal = relay
        .record_observation(RelayObservation::Warning {
            message: "restored".into(),
        })
        .unwrap();
    assert_eq!(ordinal, 43);

    drop(relay);
    let relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        relay
            .events_after(
                41,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
            .unwrap()[0]
            .ordinal,
        42
    );
}

#[test]
fn restored_relay_rebuilds_a_queued_configuration_change() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
            restored_relay_seed_path(temp.path()),
            serde_json::to_vec(&serde_json::json!({
                "event_frontier": 41,
                "event_frontier_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "queued_prompts": [
                    {
                        "command_id": "restored-config",
                        "kind": {"set_config": {"key": "model", "value": "sonnet"}},
                        "content": [{"type": "text", "text": "/model sonnet"}],
                        "queued_at_ms": 1234
                    },
                    {
                        "command_id": "restored-prompt",
                        "content": [{"type": "text", "text": "continue offline"}],
                        "queued_at_ms": 1235
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

    let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
    let claimed = relay.claim_pending_commands(true).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command_id, "restored-config");
    assert_eq!(
        claimed[0].command,
        set_config("model", "sonnet"),
        "a restored configuration change must not become a prompt"
    );

    relay
        .record_command_completed("restored-config", RelayCommandOutcome::Configured)
        .unwrap();
    assert_eq!(
        relay.claim_pending_commands(true).unwrap()[0].command_id,
        "restored-prompt"
    );
}

/// Session teardown deletes the worker root while its daemon may still be
/// alive. A durable write that recreated the root would leave a snapshot
/// with no journal behind it, and no later resume could reopen that.
#[test]
fn relay_writes_fail_instead_of_recreating_a_deleted_worker_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker-root");
    let mut relay = DurableRelay::open(&root, SESSION, "1.0.0").unwrap();
    relay
        .record_observation(RelayObservation::Warning {
            message: "before teardown".into(),
        })
        .unwrap();
    let through = relay.latest_ordinal();
    let digest = relay.latest_digest().to_owned();
    fs::remove_dir_all(&root).unwrap();

    // Acknowledging writes the snapshot and nothing else, so this is the
    // path that used to recreate the root behind teardown's back.
    let acknowledged = relay.handle(relay_request(
        "acknowledge-after-teardown",
        RelayRequest::Acknowledge {
            through_ordinal: through,
            through_digest: digest,
        },
    ));
    assert!(
        matches!(
            acknowledged.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::Internal,
                    ..
                }
            }
        ),
        "acknowledging into a removed root must fail: {:?}",
        acknowledged.body
    );
    assert!(!root.exists(), "the snapshot write resurrected the root");

    assert!(
        relay
            .record_observation(RelayObservation::Warning {
                message: "after teardown".into(),
            })
            .is_err()
    );
    assert!(!root.exists(), "the journal append resurrected the root");
}

#[test]
fn restored_relay_rejects_an_invalid_canonical_frontier() {
    for seed in [
        // Unparseable frontier.
        br#"{"event_frontier":"forty-one"}"#.to_vec(),
        // Well-formed JSON whose digest is not a relay event digest.
        br#"{"event_frontier":41,"event_frontier_digest":"nope"}"#.to_vec(),
        // A non-genesis frontier claiming the genesis digest.
        format!(
            r#"{{"event_frontier":41,"event_frontier_digest":"{RELAY_EVENT_GENESIS_DIGEST}"}}"#
        )
        .into_bytes(),
    ] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(restored_relay_seed_path(temp.path()), &seed).unwrap();
        let error = DurableRelay::open(temp.path(), SESSION, "1.0.0")
            .err()
            .expect("invalid frontier should fail");
        assert!(
            error.to_string().contains(RESTORED_RELAY_SEED_FILE),
            "{error:#}"
        );
        assert!(!temp.path().join(RELAY_STATE_FILE).exists());
    }
}

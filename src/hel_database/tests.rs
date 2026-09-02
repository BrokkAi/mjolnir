use super::*;
use crate::hel_config::HarnessKind;
use crate::hel_state::{
    HostContainerSize, ManagedWorktreeTarget, QueuedCommandKind, TranscriptBody,
};
use crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST;
use rusqlite::OptionalExtension;

#[test]
fn a_bounded_prompt_search_reports_that_it_stopped_early() {
    // Without the flag a caller cannot tell ten matches from the first ten of
    // many, and will present a partial answer as a whole one.
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    for index in 0..50 {
        record_prompt_to(
            &database,
            "session-1",
            "bundle-1",
            index,
            Some("2026-01-01T00:00:00Z"),
            &format!("ship it {index}"),
        )
        .unwrap();
    }

    let found = search_prompts_bounded_from(
        &database,
        "session-1",
        "bundle-1",
        HistoryScope::Session,
        "ship",
        10,
    )
    .unwrap();
    assert_eq!(found.entries.len(), 10);
    assert!(
        found.truncated,
        "a search that stopped early reported a complete answer"
    );

    let all = search_prompts_bounded_from(
        &database,
        "session-1",
        "bundle-1",
        HistoryScope::Session,
        "ship",
        100,
    )
    .unwrap();
    assert_eq!(all.entries.len(), 50);
    assert!(!all.truncated, "a complete answer reported itself partial");
}

/// The eviction the daemon performs depends on the typed cause reaching the
/// refresher through `Controller::load`, which is three `anyhow` hops away.
/// Nothing plumbs it; this pins that nothing has to.
#[test]
fn store_schema_mismatch_survives_the_controller_load_error_chain() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    drop(schema::open_writer(&database).unwrap());
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
        .unwrap();
    drop(connection);
    forget_verified_schema(&database);

    let error = load_state_from(&database).unwrap_err();

    let mismatch = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
        .expect("the mismatch survives every hop to the caller");
    assert_eq!(mismatch.found, SCHEMA_VERSION + 1);
    assert_eq!(mismatch.supported, SCHEMA_VERSION);
}

/// The writer verifies the schema once, when it opens. Issue #24 is what
/// happens next: another process migrated the store, and this lane kept
/// writing rows the store's new ladder does not expect. Every queued write is
/// now refused with the reason.
#[test]
fn writer_refuses_a_projection_write_after_the_store_moves() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let owner = start_database_writer_at(&database, false).unwrap();
    let writer = owner.writer.clone();
    migrate_store_underneath(&database);

    let mutation = MaterializedSessionMutation {
        last_activity_at_ms: Some(105),
        ..MaterializedSessionMutation::default()
    };
    let digest = event_digest(1);
    let failure = writer
        .execute("apply_projection_event", move |connection| {
            apply_projection_page_with(connection, "session-1", |page| {
                page.apply(1, RELAY_EVENT_GENESIS_DIGEST, &digest, &mutation)
            })
        })
        .unwrap_err();

    assert_mismatch(&failure);
    // Read back on a raw connection: the store is ahead of this build now, so
    // every reader this crate offers correctly refuses to open it.
    let applied = Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = 'session-1'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        applied,
        Some(0),
        "the refused write left the projection where it was"
    );
    owner.shutdown().unwrap();
}

#[test]
fn writer_refuses_a_read_receipt_after_the_store_moves() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let owner = start_database_writer_at(&database, false).unwrap();
    let writer = owner.writer.clone();
    migrate_store_underneath(&database);

    let failure = writer
        .execute("persist_read_receipt", move |connection| {
            persist_read_receipt_with(connection, "client-1", DEFAULT_WORKSPACE_ID, "session-1", 0)
        })
        .unwrap_err();

    assert_mismatch(&failure);
    let receipts = Connection::open(&database)
        .unwrap()
        .query_row("SELECT count(*) FROM client_read_frontiers", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(receipts, 0, "the refused receipt wrote no row");
    owner.shutdown().unwrap();
}

/// Moves the store's recorded schema forward the way another build's ladder
/// would, under a writer that has already opened it.
fn migrate_store_underneath(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
        .unwrap();
    drop(connection);
    forget_verified_schema(path);
}

fn assert_mismatch(failure: &anyhow::Error) {
    let mismatch = failure
        .chain()
        .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
        .unwrap_or_else(|| panic!("the refusal names the divergence, got {failure:#}"));
    assert_eq!(mismatch.found, SCHEMA_VERSION + 1);
    assert_eq!(mismatch.supported, SCHEMA_VERSION);
}

#[test]
fn database_writer_orders_jobs_and_survives_an_operation_error() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let owner = start_database_writer_at(&database, false).unwrap();
    let writer = owner.writer.clone();

    writer
        .execute("create_test_log", |connection| {
            connection.execute_batch(
                "CREATE TABLE writer_test_log (
                    sequence INTEGER PRIMARY KEY,
                    value TEXT NOT NULL
                 ) STRICT;",
            )?;
            Ok(())
        })
        .unwrap();
    for (sequence, value) in [(1_i64, "first"), (2, "second"), (3, "third")] {
        writer
            .execute("append_test_log", move |connection| {
                connection.execute(
                    "INSERT INTO writer_test_log(sequence, value) VALUES (?1, ?2)",
                    params![sequence, value],
                )?;
                Ok(())
            })
            .unwrap();
    }
    let failure = writer
        .execute::<(), _>("expected_failure", |connection| {
            connection.execute("INSERT INTO missing_table VALUES (1)", [])?;
            Ok(())
        })
        .unwrap_err();
    assert!(failure.to_string().contains("missing_table"));
    let values = writer
        .execute("read_test_log", |connection| {
            let mut statement =
                connection.prepare("SELECT value FROM writer_test_log ORDER BY sequence")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    assert_eq!(values, ["first", "second", "third"]);
    owner.shutdown().unwrap();
}

#[test]
fn database_writer_applies_bounded_backpressure_and_drains_accepted_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let owner = start_database_writer_at(&database, false).unwrap();
    let writer = owner.writer.clone();
    let (started_tx, started_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    writer
        .sender
        .send(DatabaseWriterMessage::Run {
            label: "block_test_writer",
            job: Box::new(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }),
        })
        .unwrap();
    started_rx.recv().unwrap();

    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for _ in 0..DATABASE_WRITE_QUEUE_CAPACITY {
        let completed = completed.clone();
        writer
            .sender
            .try_send(DatabaseWriterMessage::Run {
                label: "queued_test_write",
                job: Box::new(move |_| {
                    completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }),
            })
            .unwrap();
    }
    assert!(matches!(
        writer.sender.try_send(DatabaseWriterMessage::Run {
            label: "queue_overflow_probe",
            job: Box::new(|_| {}),
        }),
        Err(std::sync::mpsc::TrySendError::Full(_))
    ));

    release_tx.send(()).unwrap();
    owner.shutdown().unwrap();
    assert_eq!(
        completed.load(std::sync::atomic::Ordering::Relaxed),
        DATABASE_WRITE_QUEUE_CAPACITY
    );
}

#[test]
fn database_writer_reports_a_fatal_job_panic_without_hanging_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let owner = start_database_writer_at(&database, false).unwrap();
    let writer = owner.writer.clone();

    let operation = writer
        .execute::<(), _>("panic_probe", |_| panic!("intentional writer panic"))
        .unwrap_err();
    assert!(operation.to_string().contains("writer stopped"));
    let shutdown = owner.shutdown().unwrap_err();
    assert!(shutdown.to_string().contains("intentional writer panic"));
}

fn event_digest(value: u64) -> String {
    format!("{value:064x}")
}

fn session(id: &str, bundle: &str) -> SessionRecord {
    SessionRecord {
        workspace_id: DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: id.into(),
        title: "test session".into(),
        harness_kind: HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: bundle.into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "local".into(),
        resource_allocation: Some(SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 32 * 1024 * 1024 * 1024,
        }),
        additional_mounts: vec![AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            read_only: false,
        }],
        state: SessionState::Stopped,
        target: Some(TargetLocator::LocalPodman {
            container_id: "container-1".into(),
        }),
        native_session_id: Some("native-1".into()),
        acp_session_title: Some("Agent title".into()),
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T01:00:00Z".into(),
        viewed_through_event_ordinal: 7,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: Some("temporary recovery failure".into()),
        checkpoint: Some(CheckpointMetadata {
            archive_path: PathBuf::from("sessions/test.hel.zip"),
            sha256: "a".repeat(64),
            created_at: "2026-08-12T01:00:00Z".into(),
            event_frontier: 6,
        }),
    }
}

fn materialized_session(session_id: &str) -> MaterializedSession {
    MaterializedSession {
        session_id: session_id.into(),
        applied_event_ordinal: 7,
        applied_event_digest: event_digest(7),
        last_activity_at_ms: Some(1_500),
        execution: MaterializedExecutionState::Running {
            started_at_ms: 1_000,
        },
        session_title: Some("Relay refactor".into()),
        configuration: BTreeMap::from([
            ("model".into(), serde_json::json!("gpt-5.6-sol")),
            ("effort".into(), serde_json::json!("high")),
        ]),
        transcript: vec![
            Arc::new(TranscriptItem {
                stable_id: "user:1".into(),
                position: 1,
                latest_content_event_ordinal: None,
                created_at_ms: 1_000,
                last_changed_at_ms: 1_000,
                body: TranscriptBody::User {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": "build it"
                    })],
                },
            }),
            Arc::new(TranscriptItem {
                stable_id: "agent:2".into(),
                position: 2,
                latest_content_event_ordinal: Some(2),
                created_at_ms: 1_100,
                last_changed_at_ms: 1_300,
                body: TranscriptBody::Agent {
                    chunks: vec![serde_json::json!({
                        "content": {"type": "text", "text": "Working on it"},
                        "messageId": "answer-1",
                        "_meta": {"provider": "test"}
                    })],
                    streaming: false,
                },
            }),
            Arc::new(TranscriptItem {
                stable_id: "tool:call-1".into(),
                position: 3,
                latest_content_event_ordinal: None,
                created_at_ms: 1_200,
                last_changed_at_ms: 1_400,
                body: TranscriptBody::Tool {
                    call: serde_json::json!({
                        "toolCallId": "call-1",
                        "title": "Edit files",
                        "kind": "edit",
                        "status": "completed",
                        "content": [{
                            "type": "content",
                            "content": {"type": "text", "text": "done"}
                        }],
                        "locations": [{"path": "src/main.rs", "line": 4}],
                        "rawInput": {"path": "src/main.rs"},
                        "rawOutput": {"changed": true},
                        "_meta": {"provider": "test"}
                    }),
                    terminal_outputs: Vec::new(),
                    terminal_refs: Vec::new(),
                },
            }),
            Arc::new(TranscriptItem {
                stable_id: "plan:1".into(),
                position: 4,
                latest_content_event_ordinal: None,
                created_at_ms: 1_250,
                last_changed_at_ms: 1_350,
                body: TranscriptBody::Plan {
                    plan: serde_json::json!({
                        "entries": [{
                            "content": "Implement relay",
                            "priority": "high",
                            "status": "in_progress",
                            "_meta": {"provider": "test"}
                        }],
                        "_meta": {"planProvider": "test"}
                    }),
                },
            }),
        ],
        queued_prompts: vec![MaterializedQueuedPrompt {
            command_id: "prompt-2".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "then test"})],
            queued_at_ms: 1_500,
        }],
        pending_elicitations: vec![crate::hel_elicitation::ElicitationRequest {
            id: "elicitation-1".into(),
            message: "Choose one".into(),
            title: Some("Question".into()),
            description: None,
            fields: Vec::new(),
        }],
    }
}

#[test]
fn normalized_state_round_trip_preserves_children_and_order() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut state = HelState::default();
    let mut record = session("session-1", "project-1");
    record.project_directory = Some(PathBuf::from("/srv/project-1/.mj/worktrees/session-1"));
    record.managed_worktree = Some(ManagedWorktree {
        source_project_directory: PathBuf::from("/srv/project-1"),
        source_repository: PathBuf::from("/srv/project-1"),
        worktree_root: PathBuf::from("/srv/project-1/.mj/worktrees/session-1"),
        branch: "mj/session-1".into(),
        target: ManagedWorktreeTarget::Ssh {
            destination: "builder".into(),
            ssh_args: vec!["-o".into(), "BatchMode=yes".into()],
        },
    });
    record.resource_allocation = None;
    record.target = Some(TargetLocator::LocalBare {
        worker_root: PathBuf::from("/var/lib/hel/workers/session-1"),
    });
    state.sessions.insert(record.id.clone(), record);
    state.mount_history.insert(
        "local".into(),
        vec![PathBuf::from("/recent"), PathBuf::from("/older")],
    );
    state.container_sizes.insert(
        "local".into(),
        HostContainerSize {
            cpus: 12,
            memory_bytes: 48 * 1024 * 1024 * 1024,
        },
    );

    save_state_to(&database, &state).unwrap();

    assert_eq!(load_state_from(&database).unwrap(), state);
    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        connection
            .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
            .optional()
            .unwrap(),
        None
    );
}

#[test]
fn local_docker_locator_round_trips_through_the_normalized_target_table() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.target_template_id = "docker".into();
    record.target = Some(TargetLocator::LocalDocker {
        container_id: "hel-session-1".into(),
    });

    save_session_to(&database, &record).unwrap();

    let loaded = load_state_from(&database).unwrap();
    assert_eq!(loaded.sessions["session-1"], record);
    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT kind FROM session_targets WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "local-docker"
    );
}

#[test]
fn session_and_host_container_size_commit_together_and_latest_wins() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let record = session("session-1", "project-1");
    save_session_with_container_size_to(
        &database,
        &record,
        Some((
            "builder",
            HostContainerSize {
                cpus: 8,
                memory_bytes: 32,
            },
        )),
    )
    .unwrap();
    save_session_with_container_size_to(
        &database,
        &record,
        Some((
            "builder",
            HostContainerSize {
                cpus: 16,
                memory_bytes: 64,
            },
        )),
    )
    .unwrap();

    let loaded = load_state_from(&database).unwrap();
    assert_eq!(
        loaded.container_sizes["builder"],
        HostContainerSize {
            cpus: 16,
            memory_bytes: 64,
        }
    );
    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM host_container_sizes", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        1
    );
}

#[test]
fn version_fourteen_database_gains_empty_host_container_sizes() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let connection = open(&database).unwrap();
    rewind_schema_to(&connection, 14);
    drop(connection);
    forget_verified_schema(&database);

    let loaded = load_state_from(&database).unwrap();
    assert!(loaded.container_sizes.is_empty());
    let connection = open(&database).unwrap();
    // Migrating from 14 runs every later migration, so the database ends at
    // whatever the current schema is rather than at one fixed step.
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
}

#[test]
fn reopening_a_migrated_database_restores_the_client_session_state_table() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    // Simulate a database that reached the current schema version under a
    // build that predated `client_session_state`: the table is missing even
    // though nothing else needs migrating.
    connection
        .execute_batch("DROP TABLE client_session_state;")
        .unwrap();
    drop(connection);
    forget_verified_schema(&database);

    let connection = open(&database).unwrap();
    let table_exists: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'client_session_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_exists, 1);
}

#[test]
fn loading_state_does_not_restore_a_hidden_context_session_name() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut state = HelState::default();
    let mut record = session("session-1", "project-1");
    record.acp_session_title = Some("<mj-project-memory>private and truncated".into());
    state.sessions.insert(record.id.clone(), record);
    save_state_to(&database, &state).unwrap();

    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].acp_session_title,
        None
    );
}

#[test]
fn container_settings_write_overrides_mounts_and_remembered_sources() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let record = session("session-1", "project-1");
    save_session_to(&database, &record).unwrap();

    set_session_container_settings_to(
        &database,
        "session-1",
        Some("6"),
        Some("12g"),
        &[AdditionalMount {
            source: PathBuf::from("/host/models"),
            destination: PathBuf::from("/mnt/models"),
            read_only: true,
        }],
        "2026-08-13T00:00:00Z",
    )
    .unwrap();
    replace_mount_history_in(&database, "local", &[PathBuf::from("/host/models")]).unwrap();

    let loaded = load_state_from(&database).unwrap();
    let session = &loaded.sessions["session-1"];
    assert_eq!(session.container_cpus.as_deref(), Some("6"));
    assert_eq!(session.container_memory.as_deref(), Some("12g"));
    assert_eq!(
        session.additional_mounts,
        vec![AdditionalMount {
            source: PathBuf::from("/host/models"),
            destination: PathBuf::from("/mnt/models"),
            read_only: true,
        }]
    );
    assert_eq!(session.updated_at, "2026-08-13T00:00:00Z");
    assert_eq!(
        loaded.mount_history["local"],
        vec![PathBuf::from("/host/models")]
    );
}

#[test]
fn mount_read_only_round_trips_through_both_writers_and_defaults_before_the_column() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.additional_mounts = vec![
        AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            read_only: false,
        },
        AdditionalMount {
            source: PathBuf::from("/net/share"),
            destination: PathBuf::from("/mnt/share"),
            read_only: true,
        },
    ];

    save_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].additional_mounts,
        record.additional_mounts
    );

    // A database written before the column existed keeps its rows, and they
    // load as overlay mounts.
    let connection = open(&database).unwrap();
    connection
            .execute_batch(
                "ALTER TABLE session_mounts DROP COLUMN read_only;
                 DELETE FROM session_mounts;
                 INSERT INTO session_mounts(session_id, ordinal, source, destination)
                     VALUES ('session-1', 0, CAST('/net/share' AS BLOB), CAST('/mnt/share' AS BLOB));",
            )
            .unwrap();
    drop(connection);
    // Editing the schema of an already-open store is something only this
    // test does, so it has to retract the process's verification too.
    forget_verified_schema(&database);

    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].additional_mounts,
        vec![AdditionalMount {
            source: PathBuf::from("/net/share"),
            destination: PathBuf::from("/mnt/share"),
            read_only: false,
        }]
    );
}

#[test]
fn lifecycle_save_preserves_container_settings_and_mounts() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut stale = session("session-1", "project-1");
    stale.additional_mounts.clear();
    save_session_to(&database, &stale).unwrap();

    // A read-only mount proves the flag survives a stale lifecycle save too.
    let attached = AdditionalMount {
        source: PathBuf::from("/host/models"),
        destination: PathBuf::from("/mnt/models"),
        read_only: true,
    };
    set_session_container_settings_to(
        &database,
        "session-1",
        Some("6"),
        Some("12g"),
        std::slice::from_ref(&attached),
        "2026-08-15T00:00:00Z",
    )
    .unwrap();

    // The lifecycle writer still holds the record as it was before the
    // container settings were edited.
    stale.state = SessionState::Destroying;
    stale.updated_at = "2026-08-15T00:01:00Z".into();
    save_lifecycle_session_to(&database, &stale).unwrap();

    let loaded = load_state_from(&database).unwrap();
    let session = &loaded.sessions["session-1"];
    assert_eq!(session.state, SessionState::Destroying);
    assert_eq!(session.additional_mounts, vec![attached]);
    assert_eq!(session.container_cpus.as_deref(), Some("6"));
    assert_eq!(session.container_memory.as_deref(), Some("12g"));
}

#[test]
fn missing_target_keeps_a_checkpointed_session_recoverable_and_loses_one_without_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut live = session("session-1", "project-1");
    live.state = SessionState::Running;
    save_session_to(&database, &live).unwrap();

    assert_eq!(
        mark_session_target_missing_to(
            &database,
            "session-1",
            "managed container is missing",
            "2026-08-25T16:00:00Z",
        )
        .unwrap(),
        Some(SessionState::Error)
    );
    let loaded = load_state_from(&database).unwrap();
    let recoverable = &loaded.sessions["session-1"];
    assert_eq!(recoverable.state, SessionState::Error);
    assert_eq!(
        recoverable.last_error.as_deref(),
        Some("managed container is missing")
    );

    assert_eq!(
        mark_session_target_missing_to(
            &database,
            "session-1",
            "late duplicate",
            "2026-08-25T16:01:00Z",
        )
        .unwrap(),
        Some(SessionState::Error)
    );

    let mut unrecoverable = session("session-2", "project-1");
    unrecoverable.state = SessionState::Running;
    unrecoverable.checkpoint = None;
    save_session_to(&database, &unrecoverable).unwrap();
    assert_eq!(
        mark_session_target_missing_to(
            &database,
            "session-2",
            "managed container is missing",
            "2026-08-25T16:02:00Z",
        )
        .unwrap(),
        Some(SessionState::Lost)
    );

    let loaded = load_state_from(&database).unwrap();
    assert_eq!(
        loaded.sessions["session-1"].last_error.as_deref(),
        Some("late duplicate")
    );
    assert_eq!(loaded.sessions["session-2"].state, SessionState::Lost);

    assert_eq!(
        mark_session_target_missing_to(
            &database,
            "session-2",
            "late duplicate",
            "2026-08-25T16:03:00Z",
        )
        .unwrap(),
        None
    );
}

#[test]
fn version_thirteen_restores_checkpointed_lost_sessions_to_recoverable_errors() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut checkpointed = session("session-1", "project-1");
    checkpointed.state = SessionState::Lost;
    save_session_to(&database, &checkpointed).unwrap();
    let mut without_checkpoint = session("session-2", "project-1");
    without_checkpoint.state = SessionState::Lost;
    without_checkpoint.checkpoint = None;
    save_session_to(&database, &without_checkpoint).unwrap();

    let connection = Connection::open(&database).unwrap();
    rewind_schema_to(&connection, 12);
    drop(connection);
    forget_verified_schema(&database);

    let loaded = load_state_from(&database).unwrap();
    assert_eq!(loaded.sessions["session-1"].state, SessionState::Error);
    assert_eq!(loaded.sessions["session-2"].state, SessionState::Lost);
}

/// Rewinds a fixture database to `version`, removing what the migrations
/// after it created. Re-running a migration over its own table fails, so a
/// rewind has to undo the table as well as the version marker.
fn rewind_schema_to(connection: &Connection, version: i64) {
    for table in [
        "turn_review_state",
        "second_opinion_reviews",
        "second_opinion_defaults",
        "host_container_sizes",
    ] {
        connection
            .execute_batch(&format!("DROP TABLE IF EXISTS {table};"))
            .unwrap();
    }
    connection
        .execute_batch(&format!(
            "DELETE FROM schema_migrations WHERE version > {version};
             PRAGMA user_version = {version};"
        ))
        .unwrap();
}

#[test]
fn a_workspace_remembers_the_reviewer_it_last_confirmed() {
    use crate::hel_second_opinion::{HARNESS_DEFAULT_VALUE, ReviewerSelection};

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    remember_reviewer_selection_in(
        &database,
        "workspace-1",
        &ReviewerSelection {
            profile_id: "claude".into(),
            model: Some("sonnet".into()),
            effort: Some("high".into()),
        },
    )
    .unwrap();
    // A harness that advertises no model stores its default under the
    // sentinel, so the same row is preselected next time.
    remember_reviewer_selection_in(
        &database,
        "workspace-2",
        &ReviewerSelection {
            profile_id: "codex".into(),
            model: None,
            effort: None,
        },
    )
    .unwrap();

    let defaults = reviewer_defaults_in(&database).unwrap();
    assert_eq!(defaults.profile("workspace-1"), Some("claude"));
    assert_eq!(defaults.model("workspace-1", "claude"), Some("sonnet"));
    assert_eq!(
        defaults.effort("workspace-1", "claude", "sonnet"),
        Some("high")
    );
    assert_eq!(defaults.profile("workspace-2"), Some("codex"));
    assert_eq!(
        defaults.model("workspace-2", "codex"),
        Some(HARNESS_DEFAULT_VALUE)
    );
    // Workspaces do not share a reviewer.
    assert_eq!(defaults.model("workspace-2", "claude"), None);

    // Confirming a different profile replaces the workspace's choice rather
    // than leaving two remembered reviewers behind.
    remember_reviewer_selection_in(
        &database,
        "workspace-1",
        &ReviewerSelection {
            profile_id: "codex".into(),
            model: Some("deep".into()),
            effort: None,
        },
    )
    .unwrap();
    let defaults = reviewer_defaults_in(&database).unwrap();
    assert_eq!(defaults.profile("workspace-1"), Some("codex"));
    assert_eq!(defaults.model("workspace-1", "claude"), None);
    assert_eq!(defaults.model("workspace-1", "codex"), Some("deep"));
    // The other workspace is untouched.
    assert_eq!(defaults.profile("workspace-2"), Some("codex"));
}

#[test]
fn an_open_review_survives_a_restart_and_a_finished_one_does_not() {
    use crate::hel_second_opinion::ReviewWorkflow;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    assert_eq!(active_review_in(&database, "session-1").unwrap(), None);

    let (mut workflow, _) = ReviewWorkflow::start("plan-review-1", "1. Read\n2. Change", "ctx-1");
    let reviewer_transcript = vec![std::sync::Arc::new(TranscriptItem {
        stable_id: "agent:1".into(),
        position: 1,
        latest_content_event_ordinal: Some(1),
        created_at_ms: 0,
        last_changed_at_ms: 0,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": "the plan misses error handling"}
            })],
            streaming: false,
        },
    })];
    let stored = StoredReview {
        workflow: workflow.clone(),
        generation: 0,
        context_baseline: 7,
        native_lost: false,
        reviewer_transcript: reviewer_transcript.clone(),
    };
    save_active_review_in(&database, "session-1", &stored).unwrap();

    let restored = active_review_in(&database, "session-1").unwrap().unwrap();
    assert_eq!(restored, stored);
    assert_eq!(restored.workflow.proposal(), "1. Read\n2. Change");
    assert!(!restored.workflow.finished());
    // The reviewer's conversation is kept here too: its own journal dies with
    // the target, and a finished review still has to be readable.
    assert_eq!(restored.reviewer_transcript, reviewer_transcript);

    // Advancing the review updates the same row rather than adding another.
    workflow.primary_context_completed("ctx-1", "the user asked for X", "review-1");
    save_active_review_in(&database, "session-1", &StoredReview { workflow, ..stored }).unwrap();
    let restored = active_review_in(&database, "session-1").unwrap().unwrap();
    assert_eq!(restored.workflow.summary(), Some("the user asked for X"));

    clear_active_review_in(&database, "session-1").unwrap();
    assert_eq!(active_review_in(&database, "session-1").unwrap(), None);
}

#[test]
fn losing_the_target_ends_the_reviewer_conversation_and_bumps_its_generation() {
    use crate::hel_second_opinion::ReviewWorkflow;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    // Nothing to lose when no review is open.
    assert_eq!(
        lose_reviewer_continuity_in(&database, "session-1").unwrap(),
        0
    );

    let (workflow, _) = ReviewWorkflow::start("plan-review-1", "the plan", "ctx-1");
    save_active_review_in(
        &database,
        "session-1",
        &StoredReview {
            workflow,
            generation: 3,
            context_baseline: 0,
            native_lost: false,
            reviewer_transcript: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(
        lose_reviewer_continuity_in(&database, "session-1").unwrap(),
        4
    );
    let restored = active_review_in(&database, "session-1").unwrap().unwrap();
    assert!(restored.native_lost);
    assert_eq!(restored.generation, 4);
    // The captured plan is kept for reference rather than discarded with the
    // conversation.
    assert_eq!(restored.workflow.proposal(), "the plan");

    // Losing it twice must not keep bumping the generation.
    assert_eq!(
        lose_reviewer_continuity_in(&database, "session-1").unwrap(),
        4
    );
}

#[test]
fn a_workspaceless_reviewer_selection_is_refused() {
    use crate::hel_second_opinion::ReviewerSelection;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let error = remember_reviewer_selection_in(
        &database,
        "  ",
        &ReviewerSelection {
            profile_id: "codex".into(),
            model: None,
            effort: None,
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("workspace"));
}

#[test]
fn checkpointed_save_preserves_container_settings_and_mounts() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut stale = session("session-1", "project-1");
    stale.additional_mounts.clear();
    save_session_to(&database, &stale).unwrap();

    let attached = AdditionalMount {
        source: PathBuf::from("/host/models"),
        destination: PathBuf::from("/mnt/models"),
        read_only: true,
    };
    set_session_container_settings_to(
        &database,
        "session-1",
        Some("6"),
        Some("12g"),
        std::slice::from_ref(&attached),
        "2026-08-15T00:00:00Z",
    )
    .unwrap();

    let verified = CheckpointMetadata {
        archive_path: PathBuf::from("sessions/verified.hel.zip"),
        sha256: "c".repeat(64),
        created_at: "2026-08-15T00:02:00Z".into(),
        event_frontier: 21,
    };
    stale.state = SessionState::Running;
    stale.updated_at = "2026-08-15T00:02:00Z".into();
    stale.native_session_id = Some("native-checkpointed".into());
    stale.checkpoint = Some(verified.clone());
    save_checkpointed_session_to(&database, &stale).unwrap();

    let loaded = load_state_from(&database).unwrap();
    let session = &loaded.sessions["session-1"];
    assert_eq!(session.checkpoint.as_ref(), Some(&verified));
    assert_eq!(
        session.native_session_id.as_deref(),
        Some("native-checkpointed")
    );
    assert_eq!(session.additional_mounts, vec![attached]);
    assert_eq!(session.container_cpus.as_deref(), Some("6"));
    assert_eq!(session.container_memory.as_deref(), Some("12g"));
}

#[test]
fn lifecycle_save_fails_for_unknown_session() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let missing = session("session-1", "project-1");

    for error in [
        save_lifecycle_session_to(&database, &missing).unwrap_err(),
        save_checkpointed_session_to(&database, &missing).unwrap_err(),
    ] {
        assert!(
            format!("{error:#}").contains("unknown session session-1"),
            "{error:#}"
        );
    }

    assert!(load_state_from(&database).unwrap().sessions.is_empty());
}

#[test]
fn destroying_session_round_trip_is_durable() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.state = SessionState::Destroying;

    save_session_to(&database, &record).unwrap();

    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"],
        record
    );
}

#[test]
fn interrupted_checkpoint_recovery_is_field_scoped_and_one_shot() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut checkpointing = session("session-1", "project-1");
    checkpointing.state = SessionState::Checkpointing;
    checkpointing.last_error = Some("preserve this diagnostic".into());
    let mut closing = session("session-2", "project-2");
    closing.state = SessionState::Closing;
    save_session_to(&database, &checkpointing).unwrap();
    save_session_to(&database, &closing).unwrap();

    assert_eq!(
        recover_interrupted_checkpointing_sessions_to(&database, "2026-08-14T12:00:00Z").unwrap(),
        1
    );

    let recovered = load_state_from(&database).unwrap();
    let session = &recovered.sessions["session-1"];
    assert_eq!(session.state, SessionState::Running);
    assert_eq!(session.updated_at, "2026-08-14T12:00:00Z");
    assert_eq!(
        session.last_error.as_deref(),
        Some("preserve this diagnostic")
    );
    assert_eq!(session.target, checkpointing.target);
    assert_eq!(session.checkpoint, checkpointing.checkpoint);
    assert!(
        session
            .last_checkpoint_error
            .as_deref()
            .is_some_and(|error| error.contains("controller restart"))
    );
    assert_eq!(recovered.sessions["session-2"], closing);
    assert_eq!(
        recover_interrupted_checkpointing_sessions_to(&database, "2026-08-14T12:01:00Z").unwrap(),
        0
    );
}

#[test]
fn display_updates_cannot_restore_a_stale_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut stale = session("session-1", "project-1");
    stale.state = SessionState::Error;
    stale.last_error = Some("worker bootstrap failed: upload failed".into());
    save_session_to(&database, &stale).unwrap();
    let recovered = CheckpointMetadata {
        archive_path: PathBuf::from("sessions/recovered.hel.zip"),
        sha256: "b".repeat(64),
        created_at: "2026-08-14T12:00:00Z".into(),
        event_frontier: 42,
    };
    record_recovery_success_to(&database, "session-1", "native-recovered", &recovered).unwrap();

    set_session_title_override_to(
        &database,
        "session-1",
        "Renamed safely",
        "2026-08-14T12:01:00Z",
    )
    .unwrap();
    set_session_acp_title_to(&database, "session-1", Some("Harness title")).unwrap();

    let loaded = load_state_from(&database).unwrap();
    let session = &loaded.sessions["session-1"];
    assert_eq!(session.checkpoint.as_ref(), Some(&recovered));
    assert_eq!(
        session.native_session_id.as_deref(),
        Some("native-recovered")
    );
    assert_eq!(
        session.session_title_override.as_deref(),
        Some("Renamed safely")
    );
    assert_eq!(session.acp_session_title.as_deref(), Some("Harness title"));
    assert_eq!(session.state, SessionState::Error);
    assert_eq!(
        session.last_error.as_deref(),
        Some("worker bootstrap failed: upload failed")
    );

    set_session_acp_title_to(&database, "session-1", None).unwrap();
    assert!(
        load_state_from(&database).unwrap().sessions["session-1"]
            .acp_session_title
            .is_none()
    );
}

#[test]
fn lifecycle_write_preserves_independently_owned_session_fields() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut stale = session("session-1", "project-1");
    stale.native_session_id = Some("native-old".into());
    stale.acp_session_title = Some("Old harness title".into());
    stale.session_title_override = Some("Old user title".into());
    stale.checkpoint = Some(CheckpointMetadata {
        archive_path: PathBuf::from("sessions/old.hel.zip"),
        sha256: "a".repeat(64),
        created_at: "2026-08-14T11:00:00Z".into(),
        event_frontier: 10,
    });
    save_session_to(&database, &stale).unwrap();
    let recovered = CheckpointMetadata {
        archive_path: PathBuf::from("sessions/recovered.hel.zip"),
        sha256: "b".repeat(64),
        created_at: "2026-08-14T12:00:00Z".into(),
        event_frontier: 42,
    };
    record_recovery_success_to(&database, "session-1", "native-recovered", &recovered).unwrap();
    set_session_title_override_to(
        &database,
        "session-1",
        "Current user title",
        "2026-08-14T12:01:00Z",
    )
    .unwrap();
    set_session_acp_title_to(&database, "session-1", Some("Current harness title")).unwrap();

    stale.state = SessionState::Destroying;
    save_lifecycle_session_to(&database, &stale).unwrap();

    let loaded = load_state_from(&database).unwrap();
    let session = &loaded.sessions["session-1"];
    assert_eq!(session.state, SessionState::Destroying);
    assert_eq!(
        session.native_session_id.as_deref(),
        Some("native-recovered")
    );
    assert_eq!(session.checkpoint.as_ref(), Some(&recovered));
    assert_eq!(
        session.session_title_override.as_deref(),
        Some("Current user title")
    );
    assert_eq!(
        session.acp_session_title.as_deref(),
        Some("Current harness title")
    );
}

#[test]
fn version_four_database_migrates_existing_targets_and_accepts_local_bare() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY CHECK(version > 0),
                     applied_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE session_contexts (
                     session_id TEXT PRIMARY KEY,
                     bundle_id TEXT NOT NULL,
                     created_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE sessions (
                     session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
                     title TEXT NOT NULL CHECK(length(trim(title)) > 0),
                     harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi')),
                     last_profile TEXT NOT NULL,
                     target_template_id TEXT NOT NULL,
                     state TEXT NOT NULL CHECK(state IN (
                         'provisioning','running','disconnected','checkpointing','closing',
                         'archived','lost','error','destroyed-with-data-loss'
                     )),
                     native_session_id TEXT,
                     acp_session_title TEXT,
                     session_title_override TEXT,
                     updated_at TEXT NOT NULL,
                     last_viewed_event_sequence INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT,
                     resource_allocation TEXT,
                     last_checkpoint_error TEXT,
                     project_directory BLOB
                 ) STRICT;
                 CREATE TABLE session_mounts (
                     session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                     source BLOB NOT NULL,
                     destination BLOB NOT NULL,
                     PRIMARY KEY(session_id, ordinal),
                     UNIQUE(session_id, destination)
                 ) STRICT;
                 CREATE TABLE session_checkpoints (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     archive_path BLOB NOT NULL,
                     sha256 TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0)
                 ) STRICT;
                 CREATE TABLE prompt_history (
                     history_id INTEGER PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                     event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0),
                     submitted_at TEXT NOT NULL,
                     text TEXT NOT NULL CHECK(length(trim(text)) > 0),
                     UNIQUE(session_id, event_sequence)
                 ) STRICT;
                 CREATE TABLE session_targets (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     kind TEXT NOT NULL CHECK(kind IN ('local-podman','apple-container','aws-ec2','ssh-bare','ssh-podman')),
                     host TEXT,
                     resource_id TEXT,
                     address TEXT,
                     workspace BLOB,
                     worker_id TEXT
                 ) STRICT;
                 INSERT INTO schema_migrations(version, applied_at) VALUES (1, 'now'), (2, 'now'), (3, 'now'), (4, 'now');
                 INSERT INTO session_contexts VALUES ('old-session', 'project-1', 'now');
                 INSERT INTO sessions(
                     session_id, title, harness_kind, last_profile, target_template_id,
                     state, updated_at
                 ) VALUES (
                     'old-session', 'old session', 'codex', 'codex', 'local',
                     'running', 'now'
                 );
                 INSERT INTO session_targets(session_id, kind, resource_id)
                     VALUES ('old-session', 'local-podman', 'container-1');
                 PRAGMA user_version = 4;",
            )
            .unwrap();
    drop(connection);

    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert!(
        connection
            .query_row(
                "SELECT managed_worktree IS NULL FROM sessions WHERE session_id = 'old-session'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT resource_id FROM session_targets WHERE session_id = 'old-session'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "container-1"
    );
    connection
        .execute(
            "DELETE FROM session_targets WHERE session_id = 'old-session'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session_targets(session_id, kind, workspace)
                 VALUES ('old-session', 'local-bare', ?1)",
            [path_to_blob(Path::new("/var/lib/hel/workers/old-session"))],
        )
        .unwrap();
}

#[test]
fn version_five_database_establishes_new_receipt_and_seeds_projection() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;
                 CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY CHECK(version > 0),
                     applied_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE session_contexts (
                     session_id TEXT PRIMARY KEY,
                     bundle_id TEXT NOT NULL,
                     created_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE sessions (
                     session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
                     title TEXT NOT NULL CHECK(length(trim(title)) > 0),
                     harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi')),
                     last_profile TEXT NOT NULL,
                     target_template_id TEXT NOT NULL,
                     state TEXT NOT NULL CHECK(state IN (
                         'provisioning','running','disconnected','checkpointing','closing',
                         'archived','lost','error','destroyed-with-data-loss'
                     )),
                     native_session_id TEXT,
                     acp_session_title TEXT,
                     session_title_override TEXT,
                     updated_at TEXT NOT NULL,
                     last_viewed_event_sequence INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT,
                     resource_allocation TEXT,
                     last_checkpoint_error TEXT,
                     project_directory BLOB
                 ) STRICT;
                 CREATE TABLE session_mounts (
                     session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                     source BLOB NOT NULL,
                     destination BLOB NOT NULL,
                     PRIMARY KEY(session_id, ordinal),
                     UNIQUE(session_id, destination)
                 ) STRICT;
                 CREATE TABLE session_checkpoints (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     archive_path BLOB NOT NULL,
                     sha256 TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0)
                 ) STRICT;
                 CREATE TABLE prompt_history (
                     history_id INTEGER PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                     event_sequence INTEGER NOT NULL CHECK(event_sequence >= 0),
                     submitted_at TEXT NOT NULL,
                     text TEXT NOT NULL CHECK(length(trim(text)) > 0),
                     UNIQUE(session_id, event_sequence)
                 ) STRICT;
                 INSERT INTO schema_migrations(version, applied_at)
                     VALUES (1, 'now'), (2, 'now'), (3, 'now'), (4, 'now'), (5, 'now');
                 INSERT INTO session_contexts VALUES ('session-1', 'project-1', 'now');
                 INSERT INTO sessions(
                     session_id, title, harness_kind, last_profile, target_template_id,
                     state, updated_at, last_viewed_event_sequence
                 ) VALUES (
                     'session-1', 'old session', 'codex', 'codex', 'local',
                     'running', 'now', 41
                 );
                 INSERT INTO prompt_history(session_id, event_sequence, submitted_at, text)
                     VALUES ('session-1', 9, 'now', 'remember the ordinal');
                 PRAGMA user_version = 5;",
        )
        .unwrap();
    drop(connection);

    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT viewed_through_event_ordinal FROM sessions WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        0
    );
    assert!(
        connection
            .query_row(
                "SELECT managed_worktree IS NULL FROM sessions WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
    assert_eq!(
            connection
                .query_row(
                    "SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = 'session-1'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            0
        );
    assert_eq!(
        connection
            .query_row(
                "SELECT text FROM prompt_history
                     WHERE session_id = 'session-1' AND event_ordinal = 9",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "remember the ordinal"
    );
    connection
        .execute(
            "UPDATE sessions SET state = 'destroying' WHERE session_id = 'session-1'",
            [],
        )
        .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT applied_event_digest FROM materialized_sessions
                     WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        RELAY_EVENT_GENESIS_DIGEST
    );
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('materialized_sessions')
                     WHERE name = 'last_activity_at_ms'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('materialized_transcript_items')
                     WHERE name = 'latest_content_event_ordinal'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn version_seven_database_runs_the_queue_kind_and_grok_harness_migrations() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
            .execute_batch(&format!(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY CHECK(version > 0),
                     applied_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE session_contexts (
                     session_id TEXT PRIMARY KEY,
                     bundle_id TEXT NOT NULL,
                     created_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE sessions (
                     session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
                     title TEXT NOT NULL CHECK(length(trim(title)) > 0),
                     harness_kind TEXT NOT NULL CHECK(harness_kind IN ('codex','claude','kimi')),
                     last_profile TEXT NOT NULL,
                     target_template_id TEXT NOT NULL,
                     state TEXT NOT NULL CHECK(state IN (
                         'provisioning','running','disconnected','checkpointing','closing','destroying',
                         'archived','lost','error','destroyed-with-data-loss'
                     )),
                     native_session_id TEXT,
                     acp_session_title TEXT,
                     session_title_override TEXT,
                     updated_at TEXT NOT NULL,
                     detached_after_event_ordinal INTEGER NOT NULL DEFAULT 0
                         CHECK(detached_after_event_ordinal >= 0),
                     last_error TEXT,
                     resource_allocation TEXT,
                     last_checkpoint_error TEXT,
                     project_directory BLOB,
                     managed_worktree TEXT
                 ) STRICT;
                 CREATE TABLE session_targets (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     kind TEXT NOT NULL CHECK(kind IN ('local-bare','local-podman','apple-container','aws-ec2','ssh-bare','ssh-podman')),
                     host TEXT,
                     resource_id TEXT,
                     address TEXT,
                     workspace BLOB,
                     worker_id TEXT
                 ) STRICT;
                 CREATE TABLE session_mounts (
                     session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                     source BLOB NOT NULL,
                     destination BLOB NOT NULL,
                     PRIMARY KEY(session_id, ordinal),
                     UNIQUE(session_id, destination)
                 ) STRICT;
                 CREATE TABLE session_checkpoints (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     archive_path BLOB NOT NULL,
                     sha256 TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     event_frontier INTEGER NOT NULL CHECK(event_frontier >= 0)
                 ) STRICT;
                 CREATE TABLE prompt_history (
                     history_id INTEGER PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES session_contexts(session_id),
                     event_ordinal INTEGER NOT NULL CHECK(event_ordinal >= 0),
                     submitted_at TEXT NOT NULL,
                     text TEXT NOT NULL CHECK(length(trim(text)) > 0),
                     UNIQUE(session_id, event_ordinal)
                 ) STRICT;
                 CREATE TABLE materialized_sessions (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     applied_event_ordinal INTEGER NOT NULL DEFAULT 0
                         CHECK(applied_event_ordinal >= 0),
                     applied_event_digest TEXT NOT NULL
                         DEFAULT '{RELAY_EVENT_GENESIS_DIGEST}'
                         CHECK(length(applied_event_digest) = 64
                               AND applied_event_digest NOT GLOB '*[^0-9a-f]*'),
                     last_activity_at_ms INTEGER,
                     execution_state TEXT NOT NULL DEFAULT 'idle'
                         CHECK(execution_state IN ('idle','running','closing','closed')),
                     running_started_at_ms INTEGER,
                     session_title TEXT,
                     configuration_json TEXT NOT NULL DEFAULT '{{}}'
                 ) STRICT;
                 CREATE TABLE materialized_transcript_items (
                     session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
                     stable_id TEXT NOT NULL,
                     position INTEGER NOT NULL CHECK(position > 0),
                     latest_content_event_ordinal INTEGER,
                     created_at_ms INTEGER NOT NULL,
                     last_changed_at_ms INTEGER NOT NULL,
                     body_json TEXT NOT NULL,
                     PRIMARY KEY(session_id, stable_id)
                 ) STRICT;
                 CREATE TABLE materialized_queued_prompts (
                     session_id TEXT NOT NULL REFERENCES materialized_sessions(session_id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                     command_id TEXT NOT NULL,
                     content_json TEXT NOT NULL,
                     queued_at_ms INTEGER NOT NULL,
                     PRIMARY KEY(session_id, ordinal),
                     UNIQUE(session_id, command_id)
                 ) STRICT;
                 INSERT INTO schema_migrations(version, applied_at)
                     VALUES (1, 'now'), (2, 'now'), (3, 'now'), (4, 'now'), (5, 'now'),
                            (6, 'now'), (7, 'now');
                 INSERT INTO session_contexts VALUES ('session-1', 'project-1', 'now');
                 INSERT INTO sessions(
                     session_id, title, harness_kind, last_profile, target_template_id,
                     state, updated_at, detached_after_event_ordinal, last_error
                 ) VALUES (
                     'session-1', 'old session', 'kimi', 'kimi-1', 'raw-localhost',
                     'running', 'now', 12, 'nothing yet'
                 );
                 INSERT INTO session_contexts VALUES ('session-2', 'project-1', 'now');
                 INSERT INTO sessions(
                     session_id, title, harness_kind, last_profile, target_template_id,
                     state, updated_at, detached_after_event_ordinal
                 ) VALUES (
                     'session-2', 'stopped session', 'kimi', 'kimi-1', 'podman',
                     'archived', 'now', 0
                 );
                 INSERT INTO materialized_sessions(session_id) VALUES ('session-2');
                 INSERT INTO session_targets(session_id, kind, resource_id)
                     VALUES ('session-1', 'local-podman', 'container-1');
                 INSERT INTO materialized_sessions(session_id) VALUES ('session-1');
                 INSERT INTO materialized_queued_prompts(
                     session_id, ordinal, command_id, content_json, queued_at_ms
                 ) VALUES ('session-1', 0, 'queued-1', '[]', 1600);
                 PRAGMA user_version = 7;",
            ))
            .unwrap();
    drop(connection);

    let connection = open(&database).unwrap();

    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT target_template_id FROM sessions WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "localhost"
    );
    // Version 8 gave queue entries a kind. A row written before it is a
    // prompt.
    assert!(table_has_column(&connection, "materialized_queued_prompts", "kind_json").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT kind_json FROM materialized_queued_prompts
                     WHERE command_id = 'queued-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "\"prompt\""
    );
    // Version 9 rebuilt `sessions`; the existing row survives with every
    // column intact.
    let (title, harness, ordinal, error, draft): (String, String, u64, String, String) = connection
        .query_row(
            "SELECT title, harness_kind, viewed_through_event_ordinal, last_error,
                            draft_input
                     FROM sessions WHERE session_id = 'session-1'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        (
            title.as_str(),
            harness.as_str(),
            ordinal,
            error.as_str(),
            draft.as_str()
        ),
        ("old session", "kimi", 12, "nothing yet", "")
    );
    // Children still resolve through the replacement table.
    assert_eq!(
        connection
            .query_row(
                "SELECT resource_id FROM session_targets WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "container-1"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM materialized_sessions WHERE session_id = 'session-1'",
                [],
                |row| row.get::<_, u64>(0),
            )
            .unwrap(),
        1
    );

    connection
        .execute_batch(
            "INSERT INTO session_contexts(session_id, bundle_id, created_at)
                 VALUES ('session-3', 'project-1', 'now');
                 INSERT INTO sessions(
                     session_id, title, harness_kind, last_profile, target_template_id,
                     state, updated_at
                 ) VALUES (
                     'session-3', 'grok session', 'grok', 'grok-1', 'podman',
                     'running', 'now'
                 );",
        )
        .expect("a migrated database must accept a Grok Build session");

    connection
        .execute_batch(
            "INSERT INTO session_contexts(session_id, bundle_id, created_at)
                 VALUES ('session-4', 'project-1', 'now');
                 INSERT INTO sessions(
                     session_id, title, harness_kind, last_profile, target_template_id,
                     state, updated_at
                 ) VALUES (
                     'session-4', 'deepseek session', 'deepseek', 'deepseek-1', 'podman',
                     'running', 'now'
                 );",
        )
        .expect("a migrated database must accept a DeepSeek Harness session");

    // Version 10 renamed the `archived` lifecycle state to `stopped` and
    // gave sessions a display-only archived flag, defaulted off.
    let (state, archived): (String, bool) = connection
        .query_row(
            "SELECT state, archived FROM sessions WHERE session_id = 'session-2'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "stopped");
    assert!(!archived);
    connection
        .execute(
            "UPDATE sessions SET state = 'archived' WHERE session_id = 'session-2'",
            [],
        )
        .expect_err("the retired state name is no longer accepted");

    // The hidden set for native sessions lives in Hel's own database.
    connection
        .execute_batch(
            "INSERT INTO hidden_native_sessions(harness_kind, native_session_id, hidden_at)
                     VALUES ('codex', 'native-1', 'now');",
        )
        .expect("a migrated database holds the native hidden set");
}

/// Archiving is a display choice with its own writer: it must not disturb
/// the lifecycle state, checkpoint, or titles other writers own.
#[test]
fn the_archived_flag_round_trips_without_touching_other_session_fields() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut session = session("session-1", "project-1");
    save_session_to(&database, &session).unwrap();
    assert!(!load_state_from(&database).unwrap().sessions["session-1"].archived);

    set_session_archived_to(&database, "session-1", true).unwrap();
    let reloaded = load_state_from(&database).unwrap().sessions["session-1"].clone();
    assert!(reloaded.archived);
    session.archived = true;
    assert_eq!(reloaded.state, session.state);
    assert_eq!(reloaded.checkpoint, session.checkpoint);
    assert_eq!(reloaded.acp_session_title, session.acp_session_title);

    set_session_archived_to(&database, "session-1", false).unwrap();
    assert!(!load_state_from(&database).unwrap().sessions["session-1"].archived);
    assert!(set_session_archived_to(&database, "missing", true).is_err());
}

/// Hel never writes a harness home, so the hidden set for native sessions
/// is Hel's own state and is keyed per harness.
#[test]
fn the_native_hidden_set_is_keyed_by_harness_and_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    assert!(hidden_native_sessions_from(&database).unwrap().is_empty());

    set_native_session_hidden_to(&database, HarnessKind::Codex, "native-1", true).unwrap();
    set_native_session_hidden_to(&database, HarnessKind::Codex, "native-1", true).unwrap();
    set_native_session_hidden_to(&database, HarnessKind::Claude, "native-1", true).unwrap();
    assert_eq!(
        hidden_native_sessions_from(&database).unwrap(),
        BTreeSet::from([
            (HarnessKind::Claude, "native-1".to_owned()),
            (HarnessKind::Codex, "native-1".to_owned()),
        ])
    );

    set_native_session_hidden_to(&database, HarnessKind::Codex, "native-1", false).unwrap();
    assert_eq!(
        hidden_native_sessions_from(&database).unwrap(),
        BTreeSet::from([(HarnessKind::Claude, "native-1".to_owned())])
    );
    // Revealing something that was never hidden is not an error.
    set_native_session_hidden_to(&database, HarnessKind::Grok, "native-9", false).unwrap();
    assert!(set_native_session_hidden_to(&database, HarnessKind::Grok, "  ", true).is_err());
}

#[test]
fn a_fresh_database_accepts_a_session_for_every_harness_kind() {
    let directory = tempfile::tempdir().unwrap();
    let connection = open(&directory.path().join("hel.sqlite3")).unwrap();

    for (index, kind) in HarnessKind::ALL.into_iter().enumerate() {
        let session_id = format!("session-{index}");
        connection
            .execute(
                "INSERT INTO session_contexts(session_id, bundle_id, created_at)
                 VALUES (?1, 'project-1', 'now')",
                params![session_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions(
                         session_id, title, harness_kind, last_profile, target_template_id,
                         state, updated_at
                     ) VALUES (?1, ?2, ?3, 'profile-1', 'podman', 'running', 'now')",
                params![session_id, format!("{kind:?} session"), kind.id()],
            )
            .unwrap_or_else(|error| {
                panic!(
                    "the sessions harness_kind CHECK must admit {:?} ({:?}): {error}",
                    kind,
                    kind.id()
                )
            });
    }

    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM sessions", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        HarnessKind::ALL.len()
    );
}

#[test]
fn master_version_six_database_converges_to_the_relay_schema() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;
                 CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY CHECK(version > 0),
                     applied_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE session_contexts (
                     session_id TEXT PRIMARY KEY,
                     bundle_id TEXT NOT NULL,
                     created_at TEXT NOT NULL
                 ) STRICT;
                 CREATE TABLE sessions (
                     session_id TEXT PRIMARY KEY REFERENCES session_contexts(session_id),
                     title TEXT,
                     harness_kind TEXT,
                     last_profile TEXT,
                     target_template_id TEXT,
                     state TEXT,
                     native_session_id TEXT,
                     acp_session_title TEXT,
                     session_title_override TEXT,
                     updated_at TEXT,
                     last_viewed_event_sequence INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT,
                     resource_allocation TEXT,
                     last_checkpoint_error TEXT,
                     project_directory BLOB,
                     managed_worktree TEXT
                 ) STRICT;
                 CREATE TABLE session_mounts (
                     session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                     ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
                     source BLOB NOT NULL,
                     destination BLOB NOT NULL,
                     PRIMARY KEY(session_id, ordinal),
                     UNIQUE(session_id, destination)
                 ) STRICT;
                 CREATE TABLE session_checkpoints (
                     session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                     archive_path BLOB,
                     sha256 TEXT,
                     created_at TEXT,
                     event_sequence INTEGER NOT NULL DEFAULT 0
                 ) STRICT;
                 CREATE TABLE prompt_history (
                     history_id INTEGER PRIMARY KEY,
                     session_id TEXT REFERENCES session_contexts(session_id),
                     event_sequence INTEGER NOT NULL DEFAULT 0,
                     submitted_at TEXT,
                     text TEXT
                 ) STRICT;
                 INSERT INTO schema_migrations(version, applied_at)
                     VALUES (1, 'now'), (2, 'now'), (3, 'now'), (4, 'now'), (5, 'now'), (6, 'now');
                 PRAGMA user_version = 6;",
        )
        .unwrap();
    drop(connection);

    let connection = open(&database).unwrap();

    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    for (table, column) in [
        ("sessions", "viewed_through_event_ordinal"),
        ("sessions", "managed_worktree"),
        ("session_checkpoints", "event_frontier"),
        ("prompt_history", "event_ordinal"),
        ("materialized_sessions", "applied_event_digest"),
        ("materialized_sessions", "pending_elicitations_json"),
        ("materialized_queued_prompts", "kind_json"),
    ] {
        assert!(table_has_column(&connection, table, column).unwrap());
    }
}

#[test]
fn queue_entry_kinds_round_trip_and_default_to_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut materialized = materialized_session("session-1");
    materialized.queued_prompts.push(MaterializedQueuedPrompt {
        command_id: "config-1".into(),
        kind: QueuedCommandKind::SetConfig {
            key: "model".into(),
            value: "sonnet".into(),
        },
        content: vec![serde_json::json!({"type": "text", "text": "/model sonnet"})],
        queued_at_ms: 1_600,
    });
    save_materialized_session_to(&database, &materialized).unwrap();

    let loaded = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.queued_prompts, materialized.queued_prompts);
    assert_eq!(
        load_materialized_queued_prompts_from(&database).unwrap()["session-1"],
        materialized.queued_prompts
    );

    // Rows written before queue entries carried a kind load as prompts.
    let connection = open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO materialized_queued_prompts(
                     session_id, ordinal, command_id, content_json, queued_at_ms
                 ) VALUES ('session-1', 9, 'legacy-1', ?1, 1700)",
            params![serde_json::json!([{"type": "text", "text": "older"}]).to_string()],
        )
        .unwrap();
    drop(connection);

    let loaded = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.queued_prompts.last().unwrap().command_id, "legacy-1");
    assert_eq!(
        loaded.queued_prompts.last().unwrap().kind,
        QueuedCommandKind::Prompt
    );
}

#[test]
fn materialized_session_round_trip_preserves_typed_projection() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let materialized = materialized_session("session-1");

    save_materialized_session_to(&database, &materialized).unwrap();

    let loaded = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded, materialized);
    assert_eq!(loaded.last_activity_at_ms(), Some(1_500));
}

#[test]
fn materialized_summary_loads_messages_without_deserializing_full_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut materialized = materialized_session("session-1");
    materialized.transcript.extend([
        Arc::new(TranscriptItem {
            stable_id: "user:5".into(),
            position: 5,
            latest_content_event_ordinal: None,
            created_at_ms: 1_600,
            last_changed_at_ms: 1_600,
            body: TranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": "ship it"})],
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: format!("{}5", crate::hel_transcript::SESSION_RESTART_ITEM_PREFIX),
            position: 5,
            latest_content_event_ordinal: None,
            created_at_ms: 1_650,
            last_changed_at_ms: 1_650,
            body: TranscriptBody::System {
                text: crate::hel_transcript::SESSION_RESTART_TEXT.into(),
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "agent:6".into(),
            position: 6,
            latest_content_event_ordinal: Some(7),
            created_at_ms: 1_700,
            last_changed_at_ms: 1_700,
            body: TranscriptBody::Agent {
                chunks: vec![serde_json::json!({
                    "content": {"type": "text", "text": "Finished"}
                })],
                streaming: false,
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "user:7".into(),
            position: 7,
            latest_content_event_ordinal: None,
            created_at_ms: 1_800,
            last_changed_at_ms: 1_800,
            body: TranscriptBody::User {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": "one more thing"
                })],
            },
        }),
    ]);
    save_materialized_session_to(&database, &materialized).unwrap();

    // A large or damaged tool result must not be read just to build the
    // dashboard's two message snippets.
    open(&database)
        .unwrap()
        .execute(
            "UPDATE materialized_transcript_items
                 SET body_json = 'not-json' WHERE stable_id = 'tool:call-1'",
            [],
        )
        .unwrap();

    let summary = load_materialized_session_summary_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(summary.last_user_message.as_deref(), Some("one more thing"));
    assert_eq!(summary.last_agent_message.as_deref(), Some("Finished"));
    assert!(!summary.last_agent_message_follows_last_user);
    assert_eq!(summary.agent_message_latest_content_ordinals, vec![2, 7]);
    assert_eq!(summary.session_restart_event_ordinals, vec![5]);
    assert_eq!(summary.execution, materialized.execution);
    assert!(load_materialized_session_from(&database, "session-1").is_err());
}

#[test]
fn queued_prompt_loader_does_not_deserialize_transcript_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let materialized = materialized_session("session-1");
    let expected = materialized.queued_prompts.clone();
    save_materialized_session_to(&database, &materialized).unwrap();
    let connection = open(&database).unwrap();
    connection
        .execute(
            "UPDATE materialized_transcript_items SET body_json = 'not-json'",
            [],
        )
        .unwrap();
    drop(connection);

    let queues = load_materialized_queued_prompts_from(&database).unwrap();

    assert_eq!(queues.get("session-1"), Some(&expected));
    assert!(load_materialized_session_from(&database, "session-1").is_err());
}

fn tool_item(position: u64, path: &str, old_text: &str, new_text: &str) -> Arc<TranscriptItem> {
    let mut diff = agent_client_protocol::schema::v1::Diff::new(path, new_text);
    diff.old_text = Some(old_text.to_owned());
    crate::hel_diff::compact_diff(&mut diff);
    Arc::new(TranscriptItem {
        stable_id: format!("tool:call-{position}"),
        position,
        latest_content_event_ordinal: None,
        created_at_ms: 1_000,
        last_changed_at_ms: 1_000,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": format!("call-{position}"),
                "title": "Edit files",
                "kind": "edit",
                "status": "completed",
                "locations": [{"path": path}],
                "content": [
                    serde_json::to_value(
                        agent_client_protocol::schema::v1::ToolCallContent::Diff(diff),
                    )
                    .unwrap(),
                    serde_json::json!({
                        "type": "content",
                        "content": {"type": "text", "text": "x".repeat(64 * 1024)}
                    }),
                ],
                "rawInput": {"file_text": "y".repeat(64 * 1024)},
                "rawOutput": {"formatted_output": "z".repeat(64 * 1024)},
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
        },
    })
}

/// The same release, on a record written before diffs were stored as patches:
/// it holds two full copies of the file and no counts, so the counts have to be
/// computed before the copies go.
#[test]
fn releasing_a_diff_written_before_patches_keeps_its_stat() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let old_text = (0..400)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let new_text = old_text.replace("line 200\n", "line 200 edited\n");
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = 30;
    materialized.applied_event_digest = event_digest(30);
    materialized.transcript = vec![Arc::new(TranscriptItem {
        stable_id: "tool:call-10".into(),
        position: 10,
        latest_content_event_ordinal: None,
        created_at_ms: 1_000,
        last_changed_at_ms: 1_000,
        body: TranscriptBody::Tool {
            call: serde_json::json!({
                "toolCallId": "call-10",
                "title": "Edit files",
                "kind": "edit",
                "status": "completed",
                "locations": [{"path": "src/legacy.rs"}],
                "content": [{
                    "type": "diff",
                    "path": "src/legacy.rs",
                    "oldText": old_text,
                    "newText": new_text,
                }],
            }),
            terminal_outputs: Vec::new(),
            terminal_refs: Vec::new(),
        },
    })];
    save_materialized_session_to(&database, &materialized).unwrap();
    let before = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    let stat_before =
        crate::hel_transcript::materialized_tool_diffstats(&before.transcript[0]).unwrap();
    assert_eq!(stat_before, vec!["src/legacy.rs  +1 −1"]);

    let retention = compact_materialized_transcript_in(&database, "session-1", 15).unwrap();

    assert_eq!(retention.items, 1);
    assert!(retention.bytes > 4 * 1024);
    let after = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::hel_transcript::materialized_tool_diffstats(&after.transcript[0]).unwrap(),
        stat_before,
        "the counts had to be computed from the copies before they were dropped"
    );
}

/// The projection only ever grew. A checkpoint archive holds the whole
/// transcript up to its frontier, so what sits below that frontier is a second
/// copy — but only the part nobody reads back may go, and the diffstat the
/// transcript still shows must survive it.
#[test]
fn a_checkpoint_releases_the_tool_output_it_covers_and_keeps_the_diffstat() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let old_text = (0..400)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let new_text = old_text.replace("line 200\n", "line 200 edited\n");
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = 30;
    materialized.applied_event_digest = event_digest(30);
    materialized.transcript = vec![
        Arc::new(TranscriptItem {
            stable_id: "user:1".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1_000,
            last_changed_at_ms: 1_000,
            body: TranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": "edit it"})],
            },
        }),
        tool_item(10, "src/covered.rs", &old_text, &new_text),
        tool_item(20, "src/live.rs", &old_text, &new_text),
    ];
    save_materialized_session_to(&database, &materialized).unwrap();
    let before = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    let stats_before = before
        .transcript
        .iter()
        .filter_map(|item| crate::hel_transcript::materialized_tool_diffstats(item))
        .collect::<Vec<_>>();

    // A checkpoint at frontier 15 covers the first tool call, not the second.
    let retention = compact_materialized_transcript_in(&database, "session-1", 15).unwrap();

    assert_eq!(
        retention.items, 1,
        "only the covered tool call was rewritten"
    );
    assert!(retention.bytes > 128 * 1024);
    assert!(!retention.remaining);

    let after = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        after
            .transcript
            .iter()
            .filter_map(|item| crate::hel_transcript::materialized_tool_diffstats(item))
            .collect::<Vec<_>>(),
        stats_before,
        "the diffstat the transcript shows must survive the release"
    );
    assert_eq!(
        after.transcript[0], before.transcript[0],
        "a user message is never released"
    );
    assert_eq!(
        after.transcript[2], before.transcript[2],
        "a tool call the checkpoint does not cover is never released"
    );
    let TranscriptBody::Tool { call, .. } = &after.transcript[1].body else {
        panic!("expected a tool call");
    };
    assert!(call.get("rawInput").is_none());
    assert!(call.get("rawOutput").is_none());
    assert_eq!(
        call["title"], "Edit files",
        "the call still says what it did"
    );
    assert_eq!(call["status"], "completed");
    assert_eq!(call["locations"][0]["path"], "src/covered.rs");
    assert_eq!(
        call["content"].as_array().unwrap().len(),
        1,
        "only the diff stays"
    );

    // Running it again finds nothing left to release.
    assert_eq!(
        compact_materialized_transcript_in(&database, "session-1", 15).unwrap(),
        TranscriptRetention::default()
    );
}

/// The steady-state poll loads this on every change, so it must cost the
/// window rather than the history — including the two facts that live outside
/// the window, which are read rather than scanned for.
#[test]
fn the_bounded_projection_carries_a_window_and_the_facts_outside_it() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut materialized = materialized_session("session-1");
    materialized.session_title = None;
    // A turn the harness started on its own is a turn boundary too, and it
    // sits outside the window here, so the read and the scan have to agree on
    // it rather than on the user message alone.
    materialized.transcript.insert(
        2,
        Arc::new(TranscriptItem {
            stable_id: format!("{}2", crate::hel_transcript::HARNESS_TURN_ITEM_PREFIX),
            position: 2,
            latest_content_event_ordinal: None,
            created_at_ms: 1_150,
            last_changed_at_ms: 1_150,
            body: TranscriptBody::System {
                text: crate::hel_transcript::HARNESS_TURN_TEXT.into(),
            },
        }),
    );
    save_materialized_session_to(&database, &materialized).unwrap();
    let whole = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();

    let (bounded, window) = load_materialized_projection_tail_from(&database, "session-1", 2)
        .unwrap()
        .unwrap();

    assert_eq!(
        bounded.transcript.len(),
        2,
        "the window is the size that was asked for"
    );
    assert_eq!(window.omitted_items, whole.transcript.len() - 2);
    // Everything but the transcript is loaded whole: these are all bounded by
    // the session, not by its history.
    assert_eq!(bounded.applied_event_ordinal, whole.applied_event_ordinal);
    assert_eq!(bounded.applied_event_digest, whole.applied_event_digest);
    assert_eq!(bounded.configuration, whole.configuration);
    assert_eq!(bounded.queued_prompts, whole.queued_prompts);
    assert_eq!(bounded.pending_elicitations, whole.pending_elicitations);
    assert_eq!(bounded.execution, whole.execution);

    // The head is outside the window, and the two facts that live there are
    // still the ones a complete projection would have found by scanning.
    let complete = crate::hel_state::ProjectionWindow::of(&whole);
    assert_eq!(complete.omitted_items, 0);
    assert_eq!(window.provisional_title, complete.provisional_title);
    assert_eq!(
        window.latest_turn_start_position,
        complete.latest_turn_start_position
    );
    assert_eq!(
        window.latest_turn_start_position,
        Some(2),
        "the newest turn start is the harness turn, not the user message"
    );
    assert!(window.provisional_title.is_some());
    assert!(
        !bounded.transcript.iter().any(|item| item.is_turn_start()),
        "the test only proves anything if the window excludes both turn starts"
    );
}

/// Seeding a conversation shows the end of it, so the reader must cost the
/// rows it returns rather than the rows that exist. Corrupting the head proves
/// the head is never touched.
#[test]
fn the_transcript_tail_reader_returns_the_end_without_reading_the_head() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let materialized = materialized_session("session-1");
    save_materialized_session_to(&database, &materialized).unwrap();
    let connection = open(&database).unwrap();
    connection
        .execute(
            "UPDATE materialized_transcript_items SET body_json = 'not-json' WHERE position <= 2",
            [],
        )
        .unwrap();
    drop(connection);

    let tail = load_materialized_transcript_tail_from(&database, "session-1", 2).unwrap();

    assert_eq!(
        tail.iter()
            .map(|item| (item.stable_id.as_str(), item.position))
            .collect::<Vec<_>>(),
        vec![("tool:call-1", 3), ("plan:1", 4)]
    );
    // Asking for more than exists returns what exists, and the corrupt head is
    // what makes that an error rather than a short read.
    assert!(load_materialized_transcript_tail_from(&database, "session-1", 256).is_err());
    assert!(
        load_materialized_transcript_tail_from(&database, "unknown", 256)
            .unwrap()
            .is_empty()
    );
}

/// Resume compares frontiers to decide whether to rebuild a projection,
/// and clears the queue without touching the transcript when it does not.
#[test]
fn a_queue_replacement_keeps_the_projection_frontier_and_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let materialized = materialized_session("session-1");
    assert!(!materialized.queued_prompts.is_empty());
    save_materialized_session_to(&database, &materialized).unwrap();

    assert_eq!(
        materialized_event_frontier_from(&database, "session-1").unwrap(),
        Some((materialized.applied_event_ordinal, event_digest(7)))
    );
    assert_eq!(
        materialized_event_frontier_from(&database, "unknown").unwrap(),
        None
    );

    replace_materialized_queued_prompts_in(&database, "session-1", &[]).unwrap();

    let cleared = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert!(cleared.queued_prompts.is_empty());
    assert_eq!(cleared.transcript, materialized.transcript);
    assert_eq!(
        cleared.applied_event_ordinal,
        materialized.applied_event_ordinal
    );
    assert_eq!(
        cleared.applied_event_digest,
        materialized.applied_event_digest
    );

    replace_materialized_queued_prompts_in(&database, "session-1", &materialized.queued_prompts)
        .unwrap();
    assert_eq!(
        load_materialized_session_from(&database, "session-1").unwrap(),
        Some(materialized)
    );
}

#[test]
fn operational_session_updates_do_not_delete_its_projection() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut operational = session("session-1", "project-1");
    save_session_to(&database, &operational).unwrap();
    let materialized = materialized_session("session-1");
    save_materialized_session_to(&database, &materialized).unwrap();

    operational.session_title_override = Some("renamed".into());
    save_session_to(&database, &operational).unwrap();

    assert_eq!(
        load_materialized_session_from(&database, "session-1").unwrap(),
        Some(materialized)
    );
}

#[test]
fn projection_event_application_is_atomic_ordered_and_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let first_item = TranscriptItem {
        stable_id: "agent:1".into(),
        position: 1,
        latest_content_event_ordinal: Some(1),
        created_at_ms: 100,
        last_changed_at_ms: 100,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": "hel"}
            })],
            streaming: true,
        },
    };
    let first = MaterializedSessionMutation {
        last_activity_at_ms: Some(105),
        execution: Some(MaterializedExecutionState::Running { started_at_ms: 90 }),
        session_title: Some(Some("Testing".into())),
        configuration: Some(BTreeMap::from([("model".into(), serde_json::json!("sol"))])),
        transcript: vec![TranscriptMutation::Upsert(first_item.clone())],
        queued_prompts: Some(vec![MaterializedQueuedPrompt {
            command_id: "prompt-2".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "next"})],
            queued_at_ms: 105,
        }]),
        pending_elicitations: None,
    };
    let first_digest = event_digest(1);
    let second_digest = event_digest(2);
    let third_digest = event_digest(3);
    assert_eq!(
        apply_projection_event_to(
            &database,
            "session-1",
            1,
            RELAY_EVENT_GENESIS_DIGEST,
            &first_digest,
            &first,
        )
        .unwrap(),
        ProjectionApplyOutcome::Applied
    );

    let destructive_duplicate = MaterializedSessionMutation {
        transcript: vec![TranscriptMutation::Remove {
            stable_id: first_item.stable_id.clone(),
        }],
        ..MaterializedSessionMutation::default()
    };
    assert_eq!(
        apply_projection_event_to(
            &database,
            "session-1",
            1,
            RELAY_EVENT_GENESIS_DIGEST,
            &first_digest,
            &destructive_duplicate,
        )
        .unwrap(),
        ProjectionApplyOutcome::AlreadyApplied
    );
    assert!(
        apply_projection_event_to(
            &database,
            "session-1",
            1,
            RELAY_EVENT_GENESIS_DIGEST,
            &event_digest(99),
            &MaterializedSessionMutation::default(),
        )
        .unwrap_err()
        .to_string()
        .contains("digest mismatch")
    );
    assert!(
        apply_projection_event_to(
            &database,
            "session-1",
            3,
            &first_digest,
            &third_digest,
            &MaterializedSessionMutation::default()
        )
        .unwrap_err()
        .to_string()
        .contains("expected ordinal 2")
    );
    assert!(
        apply_projection_event_to(
            &database,
            "session-1",
            2,
            &event_digest(99),
            &second_digest,
            &MaterializedSessionMutation::default(),
        )
        .unwrap_err()
        .to_string()
        .contains("chain diverged")
    );

    let updated_item = TranscriptItem {
        latest_content_event_ordinal: Some(2),
        last_changed_at_ms: 120,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": "hello"}
            })],
            streaming: false,
        },
        ..first_item.clone()
    };
    apply_projection_event_to(
        &database,
        "session-1",
        2,
        &first_digest,
        &second_digest,
        &MaterializedSessionMutation {
            last_activity_at_ms: Some(120),
            transcript: vec![TranscriptMutation::Upsert(updated_item.clone())],
            ..MaterializedSessionMutation::default()
        },
    )
    .unwrap();

    let regressed_content_ordinal = TranscriptItem {
        latest_content_event_ordinal: Some(1),
        last_changed_at_ms: 130,
        ..updated_item.clone()
    };
    assert!(
        apply_projection_event_to(
            &database,
            "session-1",
            3,
            &second_digest,
            &third_digest,
            &MaterializedSessionMutation {
                last_activity_at_ms: Some(130),
                transcript: vec![TranscriptMutation::Upsert(regressed_content_ordinal)],
                ..MaterializedSessionMutation::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("latest content ordinal backwards")
    );

    let invalid_identity = TranscriptItem {
        position: 2,
        ..updated_item
    };
    assert!(
        apply_projection_event_to(
            &database,
            "session-1",
            3,
            &second_digest,
            &third_digest,
            &MaterializedSessionMutation {
                last_activity_at_ms: Some(130),
                transcript: vec![TranscriptMutation::Upsert(invalid_identity)],
                ..MaterializedSessionMutation::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("immutable identity")
    );
    let loaded = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.applied_event_ordinal, 2);
    assert_eq!(loaded.applied_event_digest, second_digest);
    assert_eq!(loaded.last_activity_at_ms(), Some(120));
    assert_eq!(loaded.transcript.len(), 1);
    assert_eq!(loaded.transcript[0].latest_content_event_ordinal, Some(2));
    assert_eq!(
        loaded.transcript[0].body,
        TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": "hello"}
            })],
            streaming: false,
        }
    );
}

#[test]
fn detach_receipt_is_monotonic_and_cannot_pass_projection() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut operational = session("session-1", "project-1");
    operational.viewed_through_event_ordinal = 0;
    save_session_to(&database, &operational).unwrap();
    let mut previous_digest = RELAY_EVENT_GENESIS_DIGEST.to_owned();
    for ordinal in 1..=2 {
        let digest = event_digest(ordinal);
        apply_projection_event_to(
            &database,
            "session-1",
            ordinal,
            &previous_digest,
            &digest,
            &MaterializedSessionMutation::default(),
        )
        .unwrap();
        previous_digest = digest;
    }

    assert_eq!(
        advance_viewed_through_event_ordinal_to(&database, "session-1", 2).unwrap(),
        2
    );
    assert_eq!(
        advance_viewed_through_event_ordinal_to(&database, "session-1", 1).unwrap(),
        2
    );
    assert!(
        advance_viewed_through_event_ordinal_to(&database, "session-1", 3)
            .unwrap_err()
            .to_string()
            .contains("projection is at 2")
    );
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].viewed_through_event_ordinal,
        2
    );

    let mut connection = open(&database).unwrap();
    assert_eq!(
        persist_read_receipt_with(
            &mut connection,
            "client-a",
            DEFAULT_WORKSPACE_ID,
            "session-1",
            2,
        )
        .unwrap(),
        2
    );
    assert!(
        persist_read_receipt_with(
            &mut connection,
            "client-a",
            DEFAULT_WORKSPACE_ID,
            "session-1",
            3,
        )
        .unwrap_err()
        .to_string()
        .contains("projection is at 2")
    );
    assert_eq!(
        client_read_frontier_at(&database, "client-a", DEFAULT_WORKSPACE_ID, "session-1").unwrap(),
        2
    );
}

#[test]
fn session_draft_input_round_trips_and_an_empty_draft_clears_it() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].draft_input,
        ""
    );

    set_session_draft_input_at(&database, "session-1", "half typed thought").unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].draft_input,
        "half typed thought"
    );

    // An ordinary session save must not roll the draft back.
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].draft_input,
        "half typed thought"
    );

    set_session_draft_input_at(&database, "session-1", "").unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].draft_input,
        ""
    );

    assert!(
        set_session_draft_input_at(&database, "missing", "text")
            .unwrap_err()
            .to_string()
            .contains("unknown session missing")
    );
}

#[test]
fn projection_activity_watermark_is_atomic_and_monotonic() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let first_digest = event_digest(1);
    apply_projection_event_to(
        &database,
        "session-1",
        1,
        RELAY_EVENT_GENESIS_DIGEST,
        &first_digest,
        &MaterializedSessionMutation {
            last_activity_at_ms: Some(500),
            queued_prompts: Some(vec![MaterializedQueuedPrompt {
                command_id: "queued-1".into(),
                kind: QueuedCommandKind::Prompt,
                content: vec![serde_json::json!({"type": "text", "text": "later"})],
                queued_at_ms: 500,
            }]),
            ..MaterializedSessionMutation::default()
        },
    )
    .unwrap();
    apply_projection_event_to(
        &database,
        "session-1",
        2,
        &first_digest,
        &event_digest(2),
        &MaterializedSessionMutation {
            last_activity_at_ms: Some(400),
            queued_prompts: Some(Vec::new()),
            ..MaterializedSessionMutation::default()
        },
    )
    .unwrap();

    let loaded = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert!(loaded.queued_prompts.is_empty());
    assert_eq!(loaded.last_activity_at_ms(), Some(500));
    assert_eq!(loaded.applied_event_ordinal, 2);
}

/// One agent message per relay event, shaped so the projection can store it.
fn agent_message_mutation(ordinal: u64) -> MaterializedSessionMutation {
    MaterializedSessionMutation {
        last_activity_at_ms: Some(1_000 + ordinal as i64),
        transcript: vec![TranscriptMutation::Upsert(TranscriptItem {
            stable_id: format!("item-{ordinal}"),
            position: ordinal,
            latest_content_event_ordinal: Some(ordinal),
            created_at_ms: 1_000 + ordinal as i64,
            last_changed_at_ms: 1_000 + ordinal as i64,
            body: TranscriptBody::Agent {
                chunks: vec![serde_json::json!({
                    "content": {"type": "text", "text": format!("event {ordinal}")}
                })],
                streaming: false,
            },
        })],
        ..MaterializedSessionMutation::default()
    }
}

#[test]
fn projection_page_advances_the_frontier_only_when_the_whole_page_commits() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    // The second event breaks the chain only after the first has written
    // its rows, so the page has to unwind work it already did.
    let interrupted = apply_projection_page_to(&database, "session-1", |page| {
        page.apply(
            1,
            RELAY_EVENT_GENESIS_DIGEST,
            &event_digest(1),
            &agent_message_mutation(1),
        )?;
        page.apply(
            3,
            &event_digest(1),
            &event_digest(3),
            &agent_message_mutation(3),
        )
    })
    .unwrap_err();
    assert!(
        interrupted.to_string().contains("expected ordinal 2"),
        "unexpected page failure: {interrupted:#}"
    );

    // The relay retains everything past the last acknowledgement, so an
    // interrupted page must leave the durable frontier where it was.
    let rolled_back = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(rolled_back.applied_event_ordinal, 0);
    assert_eq!(rolled_back.applied_event_digest, RELAY_EVENT_GENESIS_DIGEST);
    assert!(rolled_back.transcript.is_empty());
    assert_eq!(rolled_back.last_activity_at_ms(), None);

    apply_projection_page_to(&database, "session-1", |page| {
        page.apply(
            1,
            RELAY_EVENT_GENESIS_DIGEST,
            &event_digest(1),
            &agent_message_mutation(1),
        )?;
        page.apply(
            2,
            &event_digest(1),
            &event_digest(2),
            &agent_message_mutation(2),
        )
    })
    .unwrap();

    let committed = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(committed.applied_event_ordinal, 2);
    assert_eq!(committed.applied_event_digest, event_digest(2));
    assert_eq!(
        committed
            .transcript
            .iter()
            .map(|item| item.stable_id.clone())
            .collect::<Vec<_>>(),
        vec!["item-1".to_owned(), "item-2".to_owned()]
    );
    assert_eq!(committed.last_activity_at_ms(), Some(1_002));
}

#[test]
fn projection_page_coalesces_repeated_item_updates_to_the_final_value() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    let first = agent_message_mutation(1);
    let mut second = agent_message_mutation(2);
    let TranscriptMutation::Upsert(second_item) = &mut second.transcript[0] else {
        unreachable!();
    };
    second_item.stable_id = "item-1".into();
    second_item.position = 1;
    second_item.created_at_ms = 1_001;
    apply_projection_page_to(&database, "session-1", |page| {
        page.apply(1, RELAY_EVENT_GENESIS_DIGEST, &event_digest(1), &first)?;
        page.apply(2, &event_digest(1), &event_digest(2), &second)
    })
    .unwrap();

    let committed = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(committed.applied_event_ordinal, 2);
    assert_eq!(committed.transcript.len(), 1);
    assert_eq!(committed.transcript[0].stable_id, "item-1");
    assert_eq!(
        committed.transcript[0].latest_content_event_ordinal,
        Some(2)
    );
    let TranscriptBody::Agent { chunks, .. } = &committed.transcript[0].body else {
        panic!("coalesced item stayed an agent message");
    };
    assert_eq!(chunks[0]["content"]["text"], "event 2");
    assert_eq!(committed.last_activity_at_ms(), Some(1_002));
}

#[test]
fn projection_page_preserves_remove_then_reinsert_identity_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    apply_projection_event_to(
        &database,
        "session-1",
        1,
        RELAY_EVENT_GENESIS_DIGEST,
        &event_digest(1),
        &agent_message_mutation(1),
    )
    .unwrap();

    let removed = MaterializedSessionMutation {
        transcript: vec![TranscriptMutation::Remove {
            stable_id: "item-1".into(),
        }],
        ..MaterializedSessionMutation::default()
    };
    let mut reinserted = agent_message_mutation(3);
    let TranscriptMutation::Upsert(reinserted_item) = &mut reinserted.transcript[0] else {
        unreachable!();
    };
    reinserted_item.stable_id = "item-1".into();
    apply_projection_page_to(&database, "session-1", |page| {
        page.apply(2, &event_digest(1), &event_digest(2), &removed)?;
        page.apply(3, &event_digest(2), &event_digest(3), &reinserted)
    })
    .unwrap();

    let committed = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(committed.applied_event_ordinal, 3);
    assert_eq!(committed.transcript.len(), 1);
    assert_eq!(committed.transcript[0].stable_id, "item-1");
    assert_eq!(committed.transcript[0].position, 3);
    assert_eq!(committed.transcript[0].created_at_ms, 1_003);
}

/// The process caches which databases it has migrated. A database that is
/// gone and recreated under the same path must still be migrated.
#[test]
fn reopening_a_recreated_database_migrates_it_again() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let sidecar = directory.path().join(format!("hel.sqlite3{suffix}"));
        if sidecar.exists() {
            fs::remove_file(&sidecar).unwrap();
        }
    }

    save_session_to(&database, &session("session-2", "project-1")).unwrap();
    let state = load_state_from(&database).unwrap();
    assert!(state.sessions.contains_key("session-2"));
    assert!(!state.sessions.contains_key("session-1"));
}

/// Catch-up throughput: one durable commit per page instead of one per
/// event. Ignored by default because it measures wall-clock time.
#[test]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
fn projection_page_apply_outruns_per_event_apply() {
    const EVENTS: u64 = 2_000;
    let directory = tempfile::tempdir().unwrap();

    let per_event_database = directory.path().join("per-event/hel.sqlite3");
    save_session_to(&per_event_database, &session("session-1", "project-1")).unwrap();
    let started = std::time::Instant::now();
    for ordinal in 1..=EVENTS {
        apply_projection_event_to(
            &per_event_database,
            "session-1",
            ordinal,
            &event_digest(ordinal - 1),
            &event_digest(ordinal),
            &agent_message_mutation(ordinal),
        )
        .unwrap();
    }
    let per_event = started.elapsed();

    let per_page_database = directory.path().join("per-page/hel.sqlite3");
    save_session_to(&per_page_database, &session("session-1", "project-1")).unwrap();
    let started = std::time::Instant::now();
    apply_projection_page_to(&per_page_database, "session-1", |page| {
        for ordinal in 1..=EVENTS {
            page.apply(
                ordinal,
                &event_digest(ordinal - 1),
                &event_digest(ordinal),
                &agent_message_mutation(ordinal),
            )?;
        }
        Ok(())
    })
    .unwrap();
    let per_page = started.elapsed();

    println!("{EVENTS} events per-event: {per_event:?}, one page: {per_page:?}");
    assert_eq!(
        load_materialized_session_from(&per_page_database, "session-1")
            .unwrap()
            .unwrap()
            .applied_event_ordinal,
        EVENTS
    );
    assert!(
        per_page < per_event,
        "one page took {per_page:?} against {per_event:?} per event"
    );
}

#[test]
fn deleting_operational_session_retains_relational_history_context() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut state = HelState::default();
    let record = session("session-1", "project-1");
    state.sessions.insert(record.id.clone(), record);
    save_state_to(&database, &state).unwrap();
    let connection = open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
                 VALUES ('session-1', 8, '2026-08-12T02:00:00Z', 'remember this')",
            [],
        )
        .unwrap();

    state.sessions.clear();
    save_state_to(&database, &state).unwrap();

    let retained: String = connection
        .query_row(
            "SELECT text FROM prompt_history WHERE session_id = 'session-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, "remember this");
}

#[test]
fn context_rejects_reassigning_a_session_to_another_project() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut state = HelState::default();
    state
        .sessions
        .insert("session-1".into(), session("session-1", "project-1"));
    save_state_to(&database, &state).unwrap();
    state.sessions.get_mut("session-1").unwrap().bundle_id = "project-2".into();

    assert!(
        save_state_to(&database, &state)
            .unwrap_err()
            .to_string()
            .contains("already associated")
    );
}

#[test]
fn history_search_scopes_by_project_session_and_all_projects() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    for (session, bundle, sequence, text) in [
        ("session-1", "project-1", 1, "fix parser"),
        ("session-2", "project-1", 1, "fix renderer"),
        ("session-3", "project-2", 1, "fix database"),
        ("session-1", "project-1", 2, "fix parser"),
    ] {
        record_prompt_to(
            &database,
            session,
            bundle,
            sequence,
            Some("2026-08-12T00:00:00Z"),
            text,
        )
        .unwrap();
    }

    let project = search_prompts_from(
        &database,
        "session-1",
        "project-1",
        HistoryScope::Project,
        "FIX",
    )
    .unwrap();
    assert_eq!(
        project
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["fix parser", "fix renderer"]
    );
    let session = search_prompts_from(
        &database,
        "session-1",
        "project-1",
        HistoryScope::Session,
        "parser",
    )
    .unwrap();
    assert_eq!(session.len(), 1, "duplicate prompt text is suppressed");
    let all = search_prompts_from(
        &database,
        "session-1",
        "project-1",
        HistoryScope::All,
        "database",
    )
    .unwrap();
    assert_eq!(all[0].session_id, "session-3");
}

#[test]
fn rebinding_a_session_moves_its_prompt_history_to_the_new_bundle() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    record_prompt_to(
        &database,
        "session-1",
        "project-1",
        1,
        Some("2026-08-12T00:00:00Z"),
        "fix parser",
    )
    .unwrap();

    rebind_session_bundle_to(&database, "session-1", "project-2").unwrap();

    assert!(
        search_prompts_from(
            &database,
            "session-1",
            "project-1",
            HistoryScope::Project,
            "fix"
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        search_prompts_from(
            &database,
            "session-1",
            "project-2",
            HistoryScope::Project,
            "fix"
        )
        .unwrap()
        .len(),
        1
    );
    // Recording under the new bundle now succeeds where it would have been
    // refused as a bundle mismatch.
    record_prompt_to(
        &database,
        "session-1",
        "project-2",
        2,
        Some("2026-08-12T00:01:00Z"),
        "fix renderer",
    )
    .unwrap();
}

#[test]
fn prompt_recording_is_idempotent_by_session_event_ordinal() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    for _ in 0..2 {
        record_prompt_to(
            &database,
            "session-1",
            "project-1",
            7,
            Some("2026-08-12T00:00:00Z"),
            "ship it",
        )
        .unwrap();
    }
    let connection = open(&database).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM prompt_history", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn independent_session_writes_preserve_both_updates() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    save_session_to(&database, &session("session-2", "project-2")).unwrap();

    let first_database = database.clone();
    let first = std::thread::spawn(move || {
        let mut record = session("session-1", "project-1");
        record.session_title_override = Some("first changed".into());
        save_session_to(&first_database, &record).unwrap();
    });
    let second_database = database.clone();
    let second = std::thread::spawn(move || {
        let mut record = session("session-2", "project-2");
        record.session_title_override = Some("second changed".into());
        save_session_to(&second_database, &record).unwrap();
    });
    first.join().unwrap();
    second.join().unwrap();

    let state = load_state_from(&database).unwrap();
    assert_eq!(
        state.sessions["session-1"]
            .session_title_override
            .as_deref(),
        Some("first changed")
    );
    assert_eq!(
        state.sessions["session-2"]
            .session_title_override
            .as_deref(),
        Some("second changed")
    );
}

#[test]
fn legacy_json_migration_commits_before_retaining_source_backup() {
    let directory = tempfile::tempdir().unwrap();
    let legacy = directory.path().join("state.json");
    let database = directory.path().join("hel.sqlite3");
    let mut state = HelState::default();
    state
        .sessions
        .insert("session-1".into(), session("session-1", "project-1"));
    state.save_to(&legacy).unwrap();

    migrate_legacy_state_from(&legacy, &database).unwrap();

    state
        .sessions
        .get_mut("session-1")
        .unwrap()
        .viewed_through_event_ordinal = 0;
    assert_eq!(load_state_from(&database).unwrap(), state);
    assert!(!legacy.exists());
    assert!(directory.path().join("state.json.migrated-v1").exists());
}

#[test]
fn legacy_sessions_are_migrated_into_the_default_workspace_without_copying_state() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    assert_eq!(
        workspace_for_session_at(&database, "session-1").unwrap(),
        Some(DEFAULT_WORKSPACE_ID.to_owned())
    );
    let workspaces = list_workspaces_from(&database).unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, DEFAULT_WORKSPACE_ID);
    assert_eq!(workspaces[0].name, "default");
    assert_eq!(workspaces[0].session_count, 1);
}

#[test]
fn workspace_crud_enforces_unique_names_and_only_deletes_empty_workspaces() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "  Bifrost  ").unwrap();
    assert_eq!(workspace.name, "Bifrost");
    assert!(create_workspace_at(&database, "bIFROST").is_err());

    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    assign_new_session_workspace_at(&database, "session-1", &workspace.id).unwrap();
    assert!(delete_empty_workspace_at(&database, &workspace.id).is_err());
    assert!(assign_new_session_workspace_at(&database, "session-1", DEFAULT_WORKSPACE_ID).is_err());

    let empty = create_workspace_at(&database, "Empty").unwrap();
    rename_workspace_at(&database, &empty.id, "Renamed").unwrap();
    delete_empty_workspace_at(&database, &empty.id).unwrap();
    assert!(
        list_workspaces_from(&database)
            .unwrap()
            .iter()
            .all(|candidate| candidate.id != empty.id)
    );
}

#[test]
fn setup_workspace_creation_returns_the_concurrent_name_winner() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    list_workspaces_from(&database).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let create = |name: &'static str| {
        let database = database.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            create_or_get_workspace_at(&database, name).unwrap()
        })
    };
    let first = create("  Bifrost  ");
    let second = create("bIFROST");
    let winner = first.join().unwrap();
    let follower = second.join().unwrap();

    assert_eq!(follower, winner);
    assert_eq!(list_workspaces_from(&database).unwrap(), vec![winner]);
}

#[test]
fn read_frontiers_are_independent_per_client_with_the_session_cursor_as_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Readers").unwrap();
    let mut record = session("session-1", "project-1");
    record.workspace_id = workspace.id.clone();
    save_session_to(&database, &record).unwrap();

    assert_eq!(
        client_read_frontier_at(&database, "client-a", &workspace.id, "session-1").unwrap(),
        7
    );
    assert_eq!(
        advance_client_read_frontier_at(&database, "client-a", &workspace.id, "session-1", 12,)
            .unwrap(),
        12
    );
    assert_eq!(
        client_read_frontier_at(&database, "client-b", &workspace.id, "session-1").unwrap(),
        7
    );
}

#[test]
fn detached_drafts_keep_source_pid_and_workspace_without_overwriting_each_other() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Drafts").unwrap();
    let mut record = session("session-1", "project-1");
    record.workspace_id = workspace.id.clone();
    save_session_to(&database, &record).unwrap();

    save_detached_draft_at(
        &database,
        &workspace.id,
        Some("session-1"),
        "tui-client-a",
        Some(1234),
        "first unfinished thought",
    )
    .unwrap();
    save_detached_draft_at(
        &database,
        &workspace.id,
        Some("session-1"),
        "tui-client-b",
        Some(5678),
        "second unfinished thought",
    )
    .unwrap();

    let drafts = list_detached_drafts_at(&database, &workspace.id).unwrap();
    assert_eq!(drafts.len(), 2);
    assert!(drafts.iter().any(|draft| {
        draft.source == "tui-client-a"
            && draft.owner_pid == Some(1234)
            && draft.text == "first unfinished thought"
    }));
    assert!(drafts.iter().any(|draft| {
        draft.source == "tui-client-b"
            && draft.owner_pid == Some(5678)
            && draft.text == "second unfinished thought"
    }));
}

#[test]
fn review_baselines_survive_a_restart_and_a_restart_clears_a_running_review() {
    use crate::hel_review::lanes::PriorReviewContext;

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    assert_eq!(
        turn_review_state_in(&database, "session-1").unwrap(),
        TurnReviewState::default(),
        "an unreviewed session starts with no baseline"
    );

    let state = TurnReviewState {
        baselines: std::collections::BTreeMap::from([(
            std::path::PathBuf::from("/workspace/app"),
            "1234abcd".to_string(),
        )]),
        reviewed_through_ordinal: 42,
        prior_review: Some(PriorReviewContext {
            synthesis: "[P1] src/a.rs:1 -- broken".to_string(),
            evidence: Default::default(),
        }),
        active: Some("{\"phase\":\"running\"}".to_string()),
    };
    save_turn_review_state_in(&database, "session-1", &state).unwrap();
    assert_eq!(turn_review_state_in(&database, "session-1").unwrap(), state);

    // On recovery the in-flight review is dropped without advancing the
    // baseline, so the next review still covers the same changes.
    let recovered = TurnReviewState {
        active: None,
        ..state.clone()
    };
    save_turn_review_state_in(&database, "session-1", &recovered).unwrap();
    let restored = turn_review_state_in(&database, "session-1").unwrap();
    assert_eq!(restored.active, None);
    assert_eq!(restored.baselines, state.baselines);
    assert_eq!(restored.reviewed_through_ordinal, 42);
}

/// Arming review moved into `config.toml`, so the per-workspace row it used to
/// live in is dropped rather than migrated: there is no defensible way to turn
/// several workspaces' answers into one global one.
#[test]
fn migration_twenty_one_drops_the_workspace_review_settings() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    // Rewound to exactly the schema this migration follows, with the table it
    // drops present and populated, rather than through the broad rewind helper
    // that other migration tests use: only migration 21 is under test here.
    let connection = open(&database).unwrap();
    connection
        .execute_batch(
            "DELETE FROM schema_migrations WHERE version > 20;
             PRAGMA user_version = 20;
             CREATE TABLE turn_review_settings (
                 workspace_id TEXT PRIMARY KEY,
                 auto_review INTEGER NOT NULL,
                 tier TEXT NOT NULL
             ) STRICT;
             INSERT INTO turn_review_settings VALUES ('workspace-1', 1, 'extended');",
        )
        .unwrap();
    drop(connection);
    forget_verified_schema(&database);

    load_state_from(&database).unwrap();

    let connection = open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    let remaining: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'turn_review_settings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "the workspace arming row is gone");
    // Re-running the migration on an already-migrated database is safe.
    drop(connection);
    forget_verified_schema(&database);
    load_state_from(&database).unwrap();
}

/// A review interrupted by a daemon restart is cancelled, not resumed, and the
/// baseline it never advanced stays where it was.
#[test]
fn clearing_interrupted_reviews_keeps_every_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    save_session_to(&database, &session("session-2", "project-1")).unwrap();

    let baselines = std::collections::BTreeMap::from([(
        std::path::PathBuf::from("/workspace/app"),
        "1234abcd".to_string(),
    )]);
    save_turn_review_state_in(
        &database,
        "session-1",
        &TurnReviewState {
            baselines: baselines.clone(),
            reviewed_through_ordinal: 42,
            prior_review: None,
            active: Some("{\"opened_at_ordinal\":42}".to_string()),
        },
    )
    .unwrap();
    save_turn_review_state_in(
        &database,
        "session-2",
        &TurnReviewState {
            baselines: baselines.clone(),
            reviewed_through_ordinal: 7,
            prior_review: None,
            active: None,
        },
    )
    .unwrap();

    let interrupted = clear_interrupted_turn_reviews_in(&database).unwrap();
    assert_eq!(interrupted, vec!["session-1".to_string()]);

    let restored = turn_review_state_in(&database, "session-1").unwrap();
    assert_eq!(restored.active, None);
    assert_eq!(
        restored.baselines, baselines,
        "the baseline is left alone, so the next review covers the same change"
    );
    assert_eq!(restored.reviewed_through_ordinal, 42);
    assert!(
        clear_interrupted_turn_reviews_in(&database)
            .unwrap()
            .is_empty(),
        "a second sweep has nothing to clear"
    );
}

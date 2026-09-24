use super::*;
use mj_core::config::HarnessKind;
use mj_core::state::{
    HostContainerSize, ManagedCheckoutKind, ManagedWorktreeTarget, MaterializedTurn,
    MaterializedTurnOutcome, PublicationAssessment, PublicationState, QueuedCommandKind,
    TranscriptBody, TurnOutcomeKind,
};

use mj_core::relay::RELAY_EVENT_GENESIS_DIGEST;
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
    migrate_store_underneath(&database);

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
    schema::advance_test_schema(path, SCHEMA_VERSION + 1, SCHEMA_VERSION + 1);
}

fn assert_mismatch(failure: &anyhow::Error) {
    let mismatch = failure
        .chain()
        .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
        .unwrap_or_else(|| panic!("the refusal names the divergence, got {failure:#}"));
    assert_eq!(mismatch.found, SCHEMA_VERSION + 1);
    assert_eq!(mismatch.supported, SCHEMA_VERSION);
    assert_eq!(
        mismatch.reason,
        StoreSchemaMismatchReason::Incompatible {
            minimum_compatible: SCHEMA_VERSION + 1,
        }
    );
}

#[test]
fn compatible_migration_preserves_new_data_through_existing_session_writes() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("mj.sqlite3");
    let mut record = session("session-1", "project-1");
    save_session_to(&database, &record).unwrap();
    let owner = start_database_writer_at(&database, false).unwrap();
    let raw = Connection::open(&database).unwrap();
    raw.execute_batch(
        "ALTER TABLE sessions ADD COLUMN future_note TEXT;
         UPDATE sessions SET future_note = 'new feature data';
         CREATE TABLE future_feature(value TEXT NOT NULL);
         INSERT INTO future_feature VALUES ('keep');",
    )
    .unwrap();
    schema::advance_test_schema(&database, SCHEMA_VERSION + 1, SCHEMA_VERSION);

    record.title = "updated by older build".into();
    let updated = record.clone();
    owner
        .writer
        .execute("save session", move |connection| {
            let tx = connection.transaction()?;
            insert_session(&tx, &updated)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    owner.shutdown().unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id].title,
        record.title
    );
    // The ordinary path-taking writer reopens the future store too.
    record.title = "updated after reopening".into();
    save_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id].title,
        record.title
    );
    assert_eq!(
        raw.query_row(
            "SELECT future_note FROM sessions WHERE session_id = 'session-1'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "new feature data"
    );
    assert_eq!(
        raw.query_row("SELECT value FROM future_feature", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    assert_eq!(
        raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION + 1
    );
    assert_eq!(
        raw.query_row("SELECT max(version) FROM schema_migrations", [], |row| row
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        SCHEMA_VERSION + 1
    );
}

#[test]
fn writer_refuses_rollback_after_observing_a_compatible_migration() {
    for reopen in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("mj.sqlite3");
        let mut owner = start_database_writer_at(&database, false).unwrap();
        schema::advance_test_schema(&database, SCHEMA_VERSION + 1, SCHEMA_VERSION);
        if reopen {
            owner.shutdown().unwrap();
            owner = start_database_writer_at(&database, false).unwrap();
        }
        owner
            .writer
            .execute("observe compatible revision", |connection| {
                connection.execute(
                    "INSERT INTO mount_history(host, source, ordinal) VALUES ('test', ?1, 0)",
                    [b"/first".as_slice()],
                )?;
                Ok(())
            })
            .unwrap();
        let raw = Connection::open(&database).unwrap();
        raw.execute(
            "DELETE FROM schema_migrations WHERE version > ?1",
            [SCHEMA_VERSION],
        )
        .unwrap();
        raw.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
        let error = owner
            .writer
            .execute("write after rollback", |connection| {
                connection.execute("DELETE FROM mount_history", [])?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StoreSchemaMismatch>().unwrap().reason,
            StoreSchemaMismatchReason::Rollback {
                previous: SCHEMA_VERSION + 1
            }
        );
        assert_eq!(
            raw.query_row("SELECT count(*) FROM mount_history", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        owner.shutdown().unwrap();
    }
}

#[test]
fn writer_refuses_missing_compatibility_metadata_instead_of_running_the_job() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("mj.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let owner = start_database_writer_at(&database, false).unwrap();
    let raw = Connection::open(&database).unwrap();
    raw.execute_batch("DROP TABLE schema_compatibility")
        .unwrap();
    let error = owner
        .writer
        .execute("delete sessions", |connection| {
            connection.execute("DELETE FROM sessions", [])?;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreSchemaMismatch>().unwrap().reason,
        StoreSchemaMismatchReason::InvalidCompatibilityMetadata
    );
    assert_eq!(
        raw.query_row("SELECT count(*) FROM sessions", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    owner.shutdown().unwrap();
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

pub(super) fn session(id: &str, bundle: &str) -> SessionRecord {
    SessionRecord {
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        publication: None,
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
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
            access: crate::targets::MountAccess::Cow,
        }],
        state: SessionState::Stopped,
        target: Some(TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "container-1".into(),
            workspace_storage: Default::default(),
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
        active_turn: None,
        last_turn_outcome: None,
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
                    presentation: None,
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
            accepted_ordinal: None,
            command_id: "prompt-2".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "then test"})],
            queued_at_ms: 1_500,
        }],
        pending_elicitations: vec![mj_core::elicitation::ElicitationRequest {
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
    let mut state = State::default();
    let mut record = session("session-1", "project-1");
    record.project_directory = Some(PathBuf::from("/srv/project-1/.mj/worktrees/session-1"));
    record.managed_worktree = Some(ManagedWorktree {
        kind: Default::default(),
        source_project_directory: PathBuf::from("/srv/project-1"),
        source_repository: PathBuf::from("/srv/project-1"),
        worktree_root: PathBuf::from("/srv/project-1/.mj/worktrees/session-1"),
        branch: "mj/session-1".into(),
        target: ManagedWorktreeTarget::Ssh {
            destination: "builder".into(),
            ssh_args: vec!["-o".into(), "BatchMode=yes".into()],
        },
        base_commit: None,
    });
    record.resource_allocation = None;
    record.launch_base = Some("origin/main".into());
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
fn clone_publication_evidence_round_trips_and_migration_preserves_old_rows() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("publication.sqlite3");
    let mut old = session("old-session", "project-1");
    old.launch_branch = Some("main".into());
    save_session_to(&database, &old).unwrap();

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE sessions DROP COLUMN publication_json;
         ALTER TABLE sessions DROP COLUMN launch_branch;
         DELETE FROM schema_migrations WHERE version >= 47;
         UPDATE schema_compatibility SET minimum_compatible_version = 46;
         PRAGMA user_version = 46;",
        )
        .unwrap();
    drop(connection);
    schema::forget_verified_schema(&database);
    drop(schema::open_writer(&database).unwrap());
    assert_eq!(
        load_state_from(&database).unwrap().sessions["old-session"].title,
        old.title
    );

    let mut clone = session("clone-session", "project-1");
    let root = PathBuf::from("/srv/project/.mj/clones/clone-session");
    clone.project_directory = Some(root.clone());
    clone.target = None;
    clone.managed_worktree = Some(ManagedWorktree {
        kind: ManagedCheckoutKind::Clone,
        source_project_directory: PathBuf::from("/srv/project"),
        source_repository: PathBuf::from("/srv/project"),
        worktree_root: root,
        branch: "main".into(),
        target: ManagedWorktreeTarget::Local,
        base_commit: Some("1".repeat(40)),
    });
    clone.publication = Some(PublicationAssessment {
        checkpoint_sha256: "a".repeat(64),
        state: PublicationState::Published,
        dirty: false,
        stashed: false,
        saved_commits: vec!["1".repeat(40)],
        destinations: vec!["https://example.test/repository.git".into()],
        checked_at: "2026-08-12T01:00:00Z".into(),
        reason: None,
    });
    save_session_to(&database, &clone).unwrap();
    let loaded = load_state_from(&database).unwrap();
    assert_eq!(
        loaded.sessions["clone-session"].publication,
        clone.publication
    );
    assert_eq!(
        loaded.sessions["clone-session"].publication_state(),
        Some(PublicationState::Published)
    );
}

#[test]
fn local_docker_locator_round_trips_through_the_normalized_target_table() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.target_template_id = "docker".into();
    record.target = Some(TargetLocator::LocalDocker {
        borrowed_from: None,
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
fn a_borrowed_container_target_round_trips_and_a_null_column_means_owned() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.target_template_id = "podman".into();
    record.target = Some(TargetLocator::LocalPodman {
        borrowed_from: Some("parent-session".into()),
        container_id: "hel-parent-session".into(),
        workspace_storage: Default::default(),
    });

    save_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"],
        record
    );

    // A row an older build wrote, or rewrote without the column, is an
    // ordinary session-owned target.
    let connection = open(&database).unwrap();
    connection
        .execute(
            "UPDATE session_targets SET borrowed_from = NULL WHERE session_id = 'session-1'",
            [],
        )
        .unwrap();
    drop(connection);
    let Some(TargetLocator::LocalPodman { borrowed_from, .. }) =
        load_state_from(&database).unwrap().sessions["session-1"]
            .target
            .clone()
    else {
        panic!("Podman locator changed kind")
    };
    assert_eq!(borrowed_from, None);
}

#[test]
fn podman_workspace_locator_round_trips_and_legacy_null_defaults_to_container_layer() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.target = Some(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "hel-session-1".into(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: "hel-session-1-workspace".into(),
        },
    });

    save_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"],
        record
    );

    let connection = open(&database).unwrap();
    connection
        .execute(
            "UPDATE session_targets SET workspace_storage = NULL WHERE session_id = 'session-1'",
            [],
        )
        .unwrap();
    drop(connection);
    let loaded = load_state_from(&database).unwrap();
    let Some(TargetLocator::LocalPodman {
        workspace_storage, ..
    }) = loaded.sessions["session-1"].target.as_ref()
    else {
        panic!("Podman locator changed kind")
    };
    assert_eq!(
        workspace_storage,
        &mj_core::state::PodmanWorkspaceLocator::ContainerLayer
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
fn loading_state_does_not_restore_a_hidden_context_session_name() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut state = State::default();
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
            access: crate::targets::MountAccess::Ro,
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
            access: crate::targets::MountAccess::Ro,
        }]
    );
    assert_eq!(session.updated_at, "2026-08-13T00:00:00Z");
    assert_eq!(
        loaded.mount_history["local"],
        vec![PathBuf::from("/host/models")]
    );
}

#[test]
fn mount_read_only_round_trips_through_both_writers() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.additional_mounts = vec![
        AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            access: crate::targets::MountAccess::Cow,
        },
        AdditionalMount {
            source: PathBuf::from("/net/share"),
            destination: PathBuf::from("/mnt/share"),
            access: crate::targets::MountAccess::Ro,
        },
        AdditionalMount {
            source: PathBuf::from("/host/build-cache"),
            destination: PathBuf::from("/mnt/build-cache"),
            access: crate::targets::MountAccess::Rw,
        },
    ];

    save_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].additional_mounts,
        record.additional_mounts
    );
}

#[test]
fn read_write_mounts_survive_an_older_writer_rewriting_session_mounts() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.additional_mounts = vec![
        AdditionalMount {
            source: PathBuf::from("/host/build-cache"),
            destination: PathBuf::from("/mnt/build-cache"),
            access: crate::targets::MountAccess::Rw,
        },
        AdditionalMount {
            source: PathBuf::from("/host/scratch"),
            destination: PathBuf::from("/mnt/scratch"),
            access: crate::targets::MountAccess::Rw,
        },
    ];
    save_session_to(&database, &record).unwrap();

    // An older build rewrites `session_mounts` from its own view, where a
    // read-write mount is merely not read-only, and knows nothing of the
    // access table. Here it keeps the first mount and makes the second one
    // read-only.
    let connection = open(&database).unwrap();
    connection
        .execute_batch(
            "DELETE FROM session_mounts WHERE session_id = 'session-1';
             INSERT INTO session_mounts(session_id, ordinal, source, destination, read_only)
                 VALUES ('session-1', 0, CAST('/host/build-cache' AS BLOB),
                         CAST('/mnt/build-cache' AS BLOB), 0),
                        ('session-1', 1, CAST('/host/scratch' AS BLOB),
                         CAST('/mnt/scratch' AS BLOB), 1);",
        )
        .unwrap();
    drop(connection);

    let loaded = &load_state_from(&database).unwrap().sessions["session-1"].additional_mounts;
    assert_eq!(loaded[0].access, crate::targets::MountAccess::Rw);
    assert_eq!(loaded[1].access, crate::targets::MountAccess::Ro);
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
        access: crate::targets::MountAccess::Ro,
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
fn provisioning_persists_the_build_cache_it_resolved() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    save_session_to(&database, &record).unwrap();

    // Provisioning resolves the cache once the container's mounts are fixed
    // and then saves through the lifecycle path, which is the only write a
    // newly provisioned session gets.
    record.state = SessionState::Running;
    record.build_cache = Some(mj_core::state::SessionBuildCache {
        host: "ssh:morannon".into(),
        directory: PathBuf::from("/mnt/nvme/mbx"),
        max_size: Some("1000GB".into()),
        target_root: None,
    });
    save_lifecycle_session_to(&database, &record).unwrap();

    let loaded = load_state_from(&database).unwrap();
    assert_eq!(
        loaded.sessions["session-1"].build_cache, record.build_cache,
        "a resumed session must find the cache its container is already mounting"
    );
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
fn a_late_missing_workspace_report_cannot_invalidate_a_newer_session() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut live = session("session-1", "project-1");
    live.state = SessionState::Running;
    live.updated_at = "2026-09-08T00:00:00Z".into();
    save_session_to(&database, &live).unwrap();
    let observed = live.updated_at.clone();
    live.updated_at = "2026-09-08T00:01:00Z".into();
    save_session_to(&database, &live).unwrap();

    assert_eq!(
        mark_session_target_missing_if_current_to(
            &database,
            &live.id,
            "old working directory is missing",
            "2026-09-08T00:02:00Z",
            Some(&observed),
        )
        .unwrap(),
        None
    );
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&live.id].state,
        SessionState::Running
    );
    assert_eq!(
        mark_session_target_missing_if_current_to(
            &database,
            &live.id,
            "current working directory is missing",
            "2026-09-08T00:02:00Z",
            Some(&live.updated_at),
        )
        .unwrap(),
        Some(SessionState::Error)
    );
    let loaded = load_state_from(&database).unwrap();
    assert_eq!(loaded.sessions[&live.id].checkpoint, live.checkpoint);
    assert_eq!(
        loaded.sessions[&live.id].last_error.as_deref(),
        Some("current working directory is missing")
    );
}

#[test]
fn an_open_review_survives_a_restart_and_a_finished_one_does_not() {
    use mj_core::second_opinion::ReviewWorkflow;

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
    use mj_core::second_opinion::ReviewWorkflow;

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
fn checkpointed_save_preserves_container_settings_and_mounts() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut stale = session("session-1", "project-1");
    stale.additional_mounts.clear();
    save_session_to(&database, &stale).unwrap();

    let attached = AdditionalMount {
        source: PathBuf::from("/host/models"),
        destination: PathBuf::from("/mnt/models"),
        access: crate::targets::MountAccess::Ro,
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

/// The ZCode and DSH harnesses are gone but their rows remain readable, so a
/// store that still holds one must list every other session instead of failing
/// outright.
#[test]
fn a_session_for_a_removed_harness_is_skipped_without_hiding_the_others() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("mj.sqlite3");
    save_session_to(&database, &session("supported", "project")).unwrap();
    save_session_to(&database, &session("legacy-zcode", "project")).unwrap();
    save_session_to(&database, &session("legacy-deepseek", "project")).unwrap();
    let connection = open(&database).unwrap();
    for (session_id, harness) in [("legacy-zcode", "zcode"), ("legacy-deepseek", "deepseek")] {
        connection
            .execute(
                "UPDATE sessions SET harness_kind=?2 WHERE session_id=?1",
                [session_id, harness],
            )
            .expect("the CHECK constraint still tolerates the stored value");
        connection
            .execute(
                "INSERT INTO hidden_native_sessions VALUES (?1,?2,'now')",
                [harness, &format!("native-{harness}")],
            )
            .unwrap();
    }
    drop(connection);

    let state = load_state_from(&database).expect("the listing must not fail");
    assert!(state.sessions.contains_key("supported"));
    for legacy in ["legacy-zcode", "legacy-deepseek"] {
        assert!(
            !state.sessions.contains_key(legacy),
            "a session whose harness was removed cannot be resumed, so it is not listed"
        );
    }
    assert!(
        hidden_native_sessions_from(&database)
            .expect("hidden sessions must not fail")
            .is_empty()
    );
}

#[test]
fn queue_entry_kinds_round_trip_and_default_to_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut materialized = materialized_session("session-1");
    materialized.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
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
            stable_id: format!("{}5", mj_core::transcript::SESSION_RESTART_ITEM_PREFIX),
            position: 5,
            latest_content_event_ordinal: None,
            created_at_ms: 1_650,
            last_changed_at_ms: 1_650,
            body: TranscriptBody::System {
                text: mj_core::transcript::SESSION_RESTART_TEXT.into(),
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
    assert!(summary.interruption_event_ordinals.is_empty());
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
    mj_core::diff::compact_diff(&mut diff);
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
            presentation: None,
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
            presentation: None,
        },
    })];
    save_materialized_session_to(&database, &materialized).unwrap();
    let before = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    let stat_before =
        mj_transcript::transcript::materialized_tool_diffstats(&before.transcript[0]).unwrap();
    assert_eq!(stat_before, vec!["src/legacy.rs  +1 −1"]);

    let retention = compact_materialized_transcript_in(&database, "session-1", 15).unwrap();

    assert_eq!(retention.items, 1);
    assert!(retention.bytes > 4 * 1024);
    let after = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        mj_transcript::transcript::materialized_tool_diffstats(&after.transcript[0]).unwrap(),
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
        .filter_map(|item| mj_transcript::transcript::materialized_tool_diffstats(item))
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
            .filter_map(|item| mj_transcript::transcript::materialized_tool_diffstats(item))
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
            stable_id: format!("{}2", mj_core::transcript::HARNESS_TURN_ITEM_PREFIX),
            position: 2,
            latest_content_event_ordinal: None,
            created_at_ms: 1_150,
            last_changed_at_ms: 1_150,
            body: TranscriptBody::System {
                text: mj_core::transcript::HARNESS_TURN_TEXT.into(),
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
    let complete = mj_core::state::ProjectionWindow::of(&whole);
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

    let tail = read_materialized_transcript(&open_reader(&database).unwrap(), "session-1", Some(2))
        .unwrap();

    assert_eq!(
        tail.iter()
            .map(|item| (item.stable_id.as_str(), item.position))
            .collect::<Vec<_>>(),
        vec![("tool:call-1", 3), ("plan:1", 4)]
    );
    // Asking for more than exists returns what exists, and the corrupt head is
    // what makes that an error rather than a short read.
    assert!(
        read_materialized_transcript(&open_reader(&database).unwrap(), "session-1", Some(256))
            .is_err()
    );
    assert!(
        read_materialized_transcript(&open_reader(&database).unwrap(), "unknown", Some(256))
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
        clear_turn_outcome: false,
        native_agent: None,
        api_events: Vec::new(),
        config_results: vec![],
        provider_cost: None,
        active_turn: None,
        last_turn_outcome: None,
        last_activity_at_ms: Some(105),
        execution: Some(MaterializedExecutionState::Running { started_at_ms: 90 }),
        session_title: Some(Some("Testing".into())),
        configuration: Some(BTreeMap::from([("model".into(), serde_json::json!("sol"))])),
        transcript: vec![TranscriptMutation::Upsert(first_item.clone())],
        queued_prompts: Some(vec![MaterializedQueuedPrompt {
            accepted_ordinal: None,
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
fn read_receipts_survive_reopen_with_a_new_terminal_identity() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let mut record = session("session-1", "project-1");
    record.viewed_through_event_ordinal = 0;
    save_session_to(&database, &record).unwrap();
    let materialized = materialized_session("session-1");
    save_materialized_session_to(&database, &materialized).unwrap();
    let through = materialized.applied_event_ordinal;
    {
        let mut connection = open(&database).unwrap();
        persist_read_receipt_with(
            &mut connection,
            "tui-old",
            DEFAULT_WORKSPACE_ID,
            "session-1",
            through,
        )
        .unwrap();
    }
    // A lifecycle writer holding an older copy cannot erase the receipt.
    save_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions["session-1"].viewed_through_event_ordinal,
        through
    );
    assert_eq!(
        client_read_frontier_at(&database, "tui-new", DEFAULT_WORKSPACE_ID, "session-1").unwrap(),
        through
    );
    let restored = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(restored.unread_agent_messages_after(through), 0);
}

#[test]
fn interruption_summary_matches_full_projection_and_legacy_outcomes() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut materialized = materialized_session("session-1");
    materialized.last_turn_outcome = Some(MaterializedTurnOutcome {
        diagnostic: None,
        usage: None,
        command_id: "prompt".into(),
        accepted_ordinal: Some(1),
        turn_start_position: Some(1),
        completed_ordinal: 3,
        completed_at_ms: 1_500,
        outcome: TurnOutcomeKind::Interrupted {
            message: "worker restarted".into(),
        },
    });
    for with_marker in [false, true] {
        if with_marker {
            materialized.transcript.push(Arc::new(TranscriptItem {
                stable_id: format!("{}3", mj_core::transcript::WORK_INTERRUPTED_ITEM_PREFIX),
                position: 3,
                latest_content_event_ordinal: None,
                created_at_ms: 1_500,
                last_changed_at_ms: 1_500,
                body: TranscriptBody::System {
                    text: "Work interrupted".into(),
                },
            }));
        }
        save_materialized_session_to(&database, &materialized).unwrap();
        let summary = load_materialized_session_summary_from(&database, "session-1")
            .unwrap()
            .unwrap();
        let restored = load_materialized_session_from(&database, "session-1")
            .unwrap()
            .unwrap();
        assert_eq!(summary.interruption_event_ordinals, vec![3]);
        assert_eq!(
            summary.interruption_event_ordinals,
            restored.interruption_event_ordinals()
        );
        assert_eq!(restored.unread_interruptions_after(3), 0);
    }
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
                accepted_ordinal: None,
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
    let mut state = State::default();
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
    let mut state = State::default();
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

/// Launch finding H-3: no session may sit in a workspace nobody can see. A
/// store whose `default` holds a session, suspended ones included, lists it
/// as an ordinary workspace named `default`; an empty `default` is not
/// listed, and its name cannot be taken for a workspace that would then be
/// invisible.
#[test]
fn a_default_workspace_that_holds_sessions_is_listed_and_an_empty_one_is_not() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let visible = create_workspace_at(&database, "Visible").unwrap();
    assert_eq!(
        list_workspaces_from(&database)
            .unwrap()
            .iter()
            .map(|workspace| workspace.id.as_str())
            .collect::<Vec<_>>(),
        [visible.id.as_str()]
    );
    let refused = create_or_get_workspace_at(&database, "Default").unwrap_err();
    assert!(format!("{refused:#}").contains("reserved"), "{refused:#}");

    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    assert_eq!(
        workspace_for_session_at(&database, "session-1").unwrap(),
        Some(DEFAULT_WORKSPACE_ID.to_owned())
    );
    let listed = list_workspaces_from(&database).unwrap();
    let default = listed
        .iter()
        .find(|workspace| workspace.id == DEFAULT_WORKSPACE_ID)
        .expect("a default workspace holding a session is listed");
    assert_eq!(default.name, "default");
    assert_eq!(
        create_or_get_workspace_at(&database, "default").unwrap().id,
        DEFAULT_WORKSPACE_ID,
        "once listed, the name selects it like any other"
    );
}

fn workspace_pane_size_row_count(path: &Path, workspace_id: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM workspace_pane_sizes WHERE workspace_id = ?1",
            [workspace_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn workspace_pane_sizes_default_without_creating_an_absent_row() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Defaults").unwrap();

    assert_eq!(workspace_pane_size_row_count(&database, &workspace.id), 0);
    assert_eq!(
        load_workspace_pane_sizes_from(&database, &workspace.id).unwrap(),
        PaneSizes::default()
    );
    assert_eq!(
        workspace_pane_size_row_count(&database, &workspace.id),
        0,
        "loading defaults must not create a settings row"
    );
}

#[test]
fn workspace_pane_sizes_round_trip_after_reopening_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Roundtrip").unwrap();
    let sizes = PaneSizes {
        sessions: PaneSize::Maximized,
        targets: PaneSize::Minimized,
        quota: PaneSize::Standard,
    };

    save_workspace_pane_sizes_to(&database, &workspace.id, sizes).unwrap();
    drop(open(&database).unwrap());
    forget_verified_schema(&database);

    assert_eq!(
        load_workspace_pane_sizes_from(&database, &workspace.id).unwrap(),
        sizes
    );
}

#[test]
fn workspace_pane_sizes_are_isolated_between_workspaces() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let first = create_workspace_at(&database, "First").unwrap();
    let second = create_workspace_at(&database, "Second").unwrap();
    let first_sizes = PaneSizes {
        sessions: PaneSize::Minimized,
        targets: PaneSize::Maximized,
        quota: PaneSize::Standard,
    };
    let second_sizes = PaneSizes {
        sessions: PaneSize::Standard,
        targets: PaneSize::Minimized,
        quota: PaneSize::Maximized,
    };

    save_workspace_pane_sizes_to(&database, &first.id, first_sizes).unwrap();
    save_workspace_pane_sizes_to(&database, &second.id, second_sizes).unwrap();

    assert_eq!(
        load_workspace_pane_sizes_from(&database, &first.id).unwrap(),
        first_sizes
    );
    assert_eq!(
        load_workspace_pane_sizes_from(&database, &second.id).unwrap(),
        second_sizes
    );
}

#[test]
fn workspace_pane_sizes_survive_rename_and_cascade_through_both_deletions() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let renamed = create_workspace_at(&database, "Before").unwrap();
    let sizes = PaneSizes {
        sessions: PaneSize::Standard,
        targets: PaneSize::Maximized,
        quota: PaneSize::Minimized,
    };
    save_workspace_pane_sizes_to(&database, &renamed.id, sizes).unwrap();

    rename_workspace_at(&database, &renamed.id, "After").unwrap();
    assert_eq!(
        load_workspace_pane_sizes_from(&database, &renamed.id).unwrap(),
        sizes,
        "renaming changes the display name, not the stable settings owner"
    );
    delete_workspace_at(&database, &renamed.id).unwrap();
    assert_eq!(workspace_pane_size_row_count(&database, &renamed.id), 0);

    let force_deleted = create_workspace_at(&database, "Force").unwrap();
    save_workspace_pane_sizes_to(
        &database,
        &force_deleted.id,
        PaneSizes {
            sessions: PaneSize::Minimized,
            targets: PaneSize::Standard,
            quota: PaneSize::Standard,
        },
    )
    .unwrap();
    force_delete_workspace_at(&database, &force_deleted.id).unwrap();
    assert_eq!(
        workspace_pane_size_row_count(&database, &force_deleted.id),
        0
    );
}

#[test]
fn invalid_pane_size_save_preserves_the_previous_row_and_unknown_workspaces_fail() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Validated").unwrap();
    let previous = PaneSizes {
        sessions: PaneSize::Maximized,
        targets: PaneSize::Minimized,
        quota: PaneSize::Standard,
    };
    save_workspace_pane_sizes_to(&database, &workspace.id, previous).unwrap();

    let invalid = PaneSizes {
        sessions: PaneSize::Maximized,
        targets: PaneSize::Maximized,
        quota: PaneSize::Standard,
    };
    assert!(save_workspace_pane_sizes_to(&database, &workspace.id, invalid).is_err());
    assert_eq!(
        load_workspace_pane_sizes_from(&database, &workspace.id).unwrap(),
        previous
    );
    assert!(load_workspace_pane_sizes_from(&database, "missing-workspace").is_err());
    assert!(save_workspace_pane_sizes_to(&database, "missing-workspace", previous).is_err());
}

#[test]
fn malformed_persisted_pane_size_is_reported_on_read() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Malformed").unwrap();
    save_workspace_pane_sizes_to(
        &database,
        &workspace.id,
        PaneSizes {
            sessions: PaneSize::Standard,
            targets: PaneSize::Minimized,
            quota: PaneSize::Standard,
        },
    )
    .unwrap();

    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE workspace_pane_sizes SET sessions = 'corrupt' WHERE workspace_id = ?1",
            [&workspace.id],
        )
        .unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = OFF;")
        .unwrap();

    let error = load_workspace_pane_sizes_from(&database, &workspace.id).unwrap_err();
    assert!(error.to_string().contains("unknown pane size"), "{error:#}");
}

fn workspace_layout_row_count(path: &Path, workspace_id: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM workspace_layouts WHERE workspace_id = ?1",
            [workspace_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn split_layout(first_session: &str, second_session: &str) -> ConversationLayout {
    use mj_core::workspace::{LayoutNode, SplitAxis};
    ConversationLayout {
        browse: None,
        pins: Default::default(),
        root: LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.6,
            first: Box::new(LayoutNode::Pane { id: 1 }),
            second: Box::new(LayoutNode::Pane { id: 2 }),
        },
        focus: 2,
        sessions: BTreeMap::from([
            (1, first_session.to_owned()),
            (2, second_session.to_owned()),
        ]),
    }
}

#[test]
fn workspace_layout_defaults_without_creating_an_absent_row() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Defaults").unwrap();

    assert_eq!(workspace_layout_row_count(&database, &workspace.id), 0);
    assert_eq!(
        load_workspace_layout_from(&database, &workspace.id).unwrap(),
        ConversationLayout::default()
    );
    assert_eq!(
        workspace_layout_row_count(&database, &workspace.id),
        0,
        "loading the default layout must not create a settings row"
    );
}

#[test]
fn workspace_layout_round_trips_after_reopening_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Roundtrip").unwrap();
    let mut layout = split_layout("session-1", "session-2");
    layout.browse = Some(2);
    layout.pins.insert("session-1".into(), 5);

    save_workspace_layout_to(&database, &workspace.id, &layout).unwrap();
    drop(open(&database).unwrap());
    forget_verified_schema(&database);

    assert_eq!(
        load_workspace_layout_from(&database, &workspace.id).unwrap(),
        layout
    );
}

#[test]
fn workspace_layouts_are_isolated_between_workspaces() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let first = create_workspace_at(&database, "First").unwrap();
    let second = create_workspace_at(&database, "Second").unwrap();
    let first_layout = split_layout("session-1", "session-2");
    let second_layout = ConversationLayout {
        browse: None,
        pins: Default::default(),
        focus: 1,
        sessions: BTreeMap::from([(1, "session-3".to_owned())]),
        ..ConversationLayout::default()
    };

    save_workspace_layout_to(&database, &first.id, &first_layout).unwrap();
    save_workspace_layout_to(&database, &second.id, &second_layout).unwrap();

    assert_eq!(
        load_workspace_layout_from(&database, &first.id).unwrap(),
        first_layout
    );
    assert_eq!(
        load_workspace_layout_from(&database, &second.id).unwrap(),
        second_layout
    );
}

#[test]
fn workspace_layouts_cascade_through_both_deletions() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let deleted = create_workspace_at(&database, "Deleted").unwrap();
    save_workspace_layout_to(&database, &deleted.id, &split_layout("a", "b")).unwrap();
    delete_workspace_at(&database, &deleted.id).unwrap();
    assert_eq!(workspace_layout_row_count(&database, &deleted.id), 0);

    let force_deleted = create_workspace_at(&database, "Force").unwrap();
    save_workspace_layout_to(&database, &force_deleted.id, &split_layout("c", "d")).unwrap();
    force_delete_workspace_at(&database, &force_deleted.id).unwrap();
    assert_eq!(workspace_layout_row_count(&database, &force_deleted.id), 0);
}

#[test]
fn invalid_layout_save_preserves_the_previous_row_and_unknown_workspaces_fail() {
    use mj_core::workspace::LayoutNode;
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Validated").unwrap();
    let previous = split_layout("session-1", "session-2");
    save_workspace_layout_to(&database, &workspace.id, &previous).unwrap();

    let mut invalid = previous.clone();
    invalid.root = LayoutNode::Pane { id: 1 };
    assert!(
        save_workspace_layout_to(&database, &workspace.id, &invalid).is_err(),
        "a session recorded for a pane outside the tree must be refused"
    );
    assert_eq!(
        load_workspace_layout_from(&database, &workspace.id).unwrap(),
        previous
    );
    assert!(load_workspace_layout_from(&database, "missing-workspace").is_err());
    assert!(save_workspace_layout_to(&database, "missing-workspace", &previous).is_err());
}

#[test]
fn malformed_persisted_layout_is_reported_on_read() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Malformed").unwrap();
    save_workspace_layout_to(&database, &workspace.id, &split_layout("a", "b")).unwrap();

    Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE workspace_layouts SET layout = '{\"root\":{\"kind\":\"pane\",\"id\":1},\
             \"focus\":9,\"sessions\":{}}' WHERE workspace_id = ?1",
            [&workspace.id],
        )
        .unwrap();

    let error = load_workspace_layout_from(&database, &workspace.id).unwrap_err();
    assert!(error.to_string().contains("focused pane 9"), "{error:#}");
}

#[test]
fn workspace_crud_preserves_history_and_blocks_active_sessions_and_drafts() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "  Bifrost  ").unwrap();
    assert_eq!(workspace.name, "Bifrost");
    assert!(create_workspace_at(&database, "bIFROST").is_err());

    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    assign_new_session_workspace_at(&database, "session-1", &workspace.id).unwrap();
    let materialized = materialized_session("session-1");
    save_materialized_session_to(&database, &materialized).unwrap();
    record_prompt_to(
        &database,
        "session-1",
        "project-1",
        1,
        Some("2026-09-03T00:00:00Z"),
        "remember this prompt",
    )
    .unwrap();
    assert!(assign_new_session_workspace_at(&database, "session-1", DEFAULT_WORKSPACE_ID).is_err());
    assert_eq!(list_workspaces_from(&database).unwrap()[0].session_count, 0);

    delete_workspace_at(&database, &workspace.id).unwrap();
    let preserved = load_state_from(&database).unwrap();
    assert_eq!(preserved.sessions["session-1"].workspace_id, workspace.id);
    assert_eq!(
        preserved.sessions["session-1"].checkpoint,
        session("session-1", "project-1").checkpoint
    );
    assert_eq!(
        load_materialized_session_from(&database, "session-1")
            .unwrap()
            .unwrap(),
        materialized
    );
    assert_eq!(
        search_prompts_from(
            &database,
            "session-1",
            "project-1",
            HistoryScope::Session,
            "remember this prompt",
        )
        .unwrap()
        .len(),
        1
    );

    let active_workspace = create_workspace_at(&database, "Active").unwrap();
    let mut active = session("session-active", "project-1");
    active.workspace_id = active_workspace.id.clone();
    active.state = SessionState::Running;
    save_session_to(&database, &active).unwrap();
    let error = delete_workspace_at(&database, &active_workspace.id).unwrap_err();
    assert!(
        error.to_string().contains("1 active sessions, 0 drafts"),
        "{error:#}"
    );

    let draft_workspace = create_workspace_at(&database, "Drafts").unwrap();
    save_detached_draft_at(
        &database,
        &draft_workspace.id,
        None,
        "terminal",
        Some(42),
        "unfinished",
    )
    .unwrap();
    let error = delete_workspace_at(&database, &draft_workspace.id).unwrap_err();
    assert!(
        error.to_string().contains("0 active sessions, 1 drafts"),
        "{error:#}"
    );

    let empty = create_workspace_at(&database, "Empty").unwrap();
    rename_workspace_at(&database, &empty.id, "Renamed").unwrap();
    delete_workspace_at(&database, &empty.id).unwrap();
    assert!(
        list_workspaces_from(&database)
            .unwrap()
            .iter()
            .all(|candidate| candidate.id != empty.id)
    );
}

#[test]
fn force_delete_workspace_drops_drafts_and_preserves_stopped_histories() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Force").unwrap();

    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    assign_new_session_workspace_at(&database, "session-1", &workspace.id).unwrap();
    let materialized = materialized_session("session-1");
    save_materialized_session_to(&database, &materialized).unwrap();
    record_prompt_to(
        &database,
        "session-1",
        "project-1",
        1,
        Some("2026-09-03T00:00:00Z"),
        "remember this prompt",
    )
    .unwrap();
    save_detached_draft_at(
        &database,
        &workspace.id,
        None,
        "terminal",
        Some(42),
        "unfinished",
    )
    .unwrap();

    force_delete_workspace_at(&database, &workspace.id).unwrap();

    assert!(
        list_detached_drafts_at(&database, &workspace.id)
            .unwrap()
            .is_empty()
    );
    let preserved = load_state_from(&database).unwrap();
    assert_eq!(preserved.sessions["session-1"].workspace_id, workspace.id);
    assert_eq!(
        preserved.sessions["session-1"].checkpoint,
        session("session-1", "project-1").checkpoint
    );
    assert_eq!(
        load_materialized_session_from(&database, "session-1")
            .unwrap()
            .unwrap(),
        materialized
    );
    assert_eq!(
        search_prompts_from(
            &database,
            "session-1",
            "project-1",
            HistoryScope::Session,
            "remember this prompt",
        )
        .unwrap()
        .len(),
        1
    );
    assert!(
        list_workspaces_from(&database)
            .unwrap()
            .iter()
            .all(|candidate| candidate.id != workspace.id)
    );
}

#[test]
fn force_delete_workspace_refuses_remaining_active_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Active").unwrap();
    let mut active = session("session-active", "project-1");
    active.workspace_id = workspace.id.clone();
    active.state = SessionState::Running;
    save_session_to(&database, &active).unwrap();
    save_detached_draft_at(
        &database,
        &workspace.id,
        None,
        "terminal",
        Some(42),
        "unfinished",
    )
    .unwrap();

    let error = force_delete_workspace_at(&database, &workspace.id).unwrap_err();
    assert!(
        error.to_string().contains("1 active sessions remain"),
        "{error:#}"
    );
    assert_eq!(
        list_detached_drafts_at(&database, &workspace.id)
            .unwrap()
            .len(),
        1,
        "a refused deletion must not drop the workspace's drafts"
    );
}

#[test]
fn only_resumable_sessions_can_move_to_a_new_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let source = create_workspace_at(&database, "Source").unwrap();
    let destination = create_workspace_at(&database, "Destination").unwrap();
    let mut stopped = session("session-stopped", "project-1");
    stopped.workspace_id = source.id.clone();
    save_session_to(&database, &stopped).unwrap();

    reassign_resumable_session_workspace_at(&database, &stopped.id, &destination.id).unwrap();
    assert_eq!(
        workspace_for_session_at(&database, &stopped.id).unwrap(),
        Some(destination.id.clone())
    );
    reassign_resumable_session_workspace_at(&database, &stopped.id, &destination.id).unwrap();

    let mut running = session("session-running", "project-1");
    running.workspace_id = source.id;
    running.state = SessionState::Running;
    save_session_to(&database, &running).unwrap();
    assert!(
        reassign_resumable_session_workspace_at(&database, &running.id, &destination.id).is_err()
    );
    assert!(
        reassign_resumable_session_workspace_at(&database, &stopped.id, "missing-workspace")
            .is_err()
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
fn detaching_retires_inherited_input_even_after_the_composer_is_cleared() {
    for current in ["", "an edited unfinished thought"] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("hel.sqlite3");
        let workspace = create_workspace_at(&database, "Drafts").unwrap();
        let mut record = session("session-1", "project-1");
        record.workspace_id = workspace.id.clone();
        record.draft_input = "/model g".into();
        save_session_to(&database, &record).unwrap();
        set_session_draft_input_at(&database, &record.id, &record.draft_input).unwrap();

        let saved = save_detached_session_draft_in(
            &mut open(&database).unwrap(),
            &workspace.id,
            &record.id,
            "tui-client-a",
            1234,
            &DetachedSessionDraft {
                text: current.into(),
                inherited_input: Some(record.draft_input.clone()),
            },
        )
        .unwrap();

        let reloaded = load_state_from(&database).unwrap();
        assert!(reloaded.sessions[&record.id].draft_input.is_empty());
        let drafts = list_detached_drafts_at(&database, &workspace.id).unwrap();
        if current.is_empty() {
            assert!(saved.is_none());
            assert!(drafts.is_empty());
        } else {
            assert_eq!(drafts.len(), 1);
            assert_eq!(drafts[0].text, current);
            assert_eq!(drafts[0].source, "tui-client-a");
            assert_eq!(drafts[0].owner_pid, Some(1234));
            // Explicit recovery is still supported after retiring the seed.
            recover_detached_draft_at(&database, &saved.unwrap()).unwrap();
            assert_eq!(
                load_state_from(&database).unwrap().sessions[&record.id].draft_input,
                current
            );
        }
    }
}

#[test]
fn detaching_preserves_a_newer_recovery_and_rolls_back_when_saving_fails() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let workspace = create_workspace_at(&database, "Drafts").unwrap();
    let mut record = session("session-1", "project-1");
    record.workspace_id = workspace.id.clone();
    record.draft_input = "newly recovered input".into();
    save_session_to(&database, &record).unwrap();
    set_session_draft_input_at(&database, &record.id, &record.draft_input).unwrap();
    let mut connection = open(&database).unwrap();

    save_detached_session_draft_in(
        &mut connection,
        &workspace.id,
        &record.id,
        "tui-client-a",
        1234,
        &DetachedSessionDraft {
            text: String::new(),
            inherited_input: Some("older inherited input".into()),
        },
    )
    .unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id].draft_input,
        record.draft_input
    );

    let result = save_detached_session_draft_in(
        &mut connection,
        &workspace.id,
        &record.id,
        "",
        1234,
        &DetachedSessionDraft {
            text: "must survive the failed save".into(),
            inherited_input: Some(record.draft_input.clone()),
        },
    );
    assert!(result.is_err());
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id].draft_input,
        record.draft_input
    );
    assert!(
        list_detached_drafts_at(&database, &workspace.id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_detached_client_cannot_retire_a_draft_in_another_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    let owner = create_workspace_at(&database, "Owner").unwrap();
    let previous = create_workspace_at(&database, "Previous").unwrap();
    let mut record = session("session-1", "project-1");
    record.workspace_id = owner.id.clone();
    save_session_to(&database, &record).unwrap();
    set_session_draft_input_at(&database, &record.id, "inherited input").unwrap();

    for text in ["", "unsent in the previous workspace"] {
        save_detached_session_draft_in(
            &mut open(&database).unwrap(),
            &previous.id,
            &record.id,
            "old-client",
            1234,
            &DetachedSessionDraft {
                text: text.into(),
                inherited_input: Some("inherited input".into()),
            },
        )
        .unwrap();
        assert_eq!(
            load_state_from(&database).unwrap().sessions[&record.id].draft_input,
            "inherited input"
        );
    }
    // Draft durability stays independent of stale workspace/read receipts.
    let drafts = list_detached_drafts_at(&database, &previous.id).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].text, "unsent in the previous workspace");
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
    use mj_core::review::driver::PendingForward;
    use mj_core::review::lanes::PriorReviewContext;

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
        pending_forward: Some(PendingForward {
            synthesis: "[P1] src/a.rs:1 -- broken".to_string(),
            evidence: Default::default(),
            command_id: "turn-review-forward-1".to_string(),
            trees: std::collections::BTreeMap::from([(
                std::path::PathBuf::from("/workspace/app"),
                "5678efgh".to_string(),
            )]),
            reviewed_through_ordinal: 43,
        }),
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
    assert_eq!(restored.pending_forward, state.pending_forward);
}

/// A review interrupted by a daemon restart is cancelled, not resumed, and the
/// baseline it never advanced stays where it was.
#[test]
fn clearing_interrupted_reviews_keeps_every_baseline() {
    use mj_core::review::driver::PendingForward;
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
            pending_forward: Some(PendingForward {
                synthesis: "[P1] broken".to_string(),
                evidence: Default::default(),
                command_id: "forward-1".to_string(),
                trees: baselines.clone(),
                reviewed_through_ordinal: 42,
            }),
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
            pending_forward: None,
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
    assert!(restored.pending_forward.is_some());
    assert_eq!(
        clear_interrupted_turn_reviews_in(&database).unwrap(),
        vec!["session-1".to_string()],
        "a pending handoff remains recoverable until its command is accepted"
    );
}

/// A projection page must carry the whole turn record, not just the transcript
/// rows: the wait endpoint reads the outcome back from these columns long after
/// the session's actor is gone.
#[test]
fn a_projection_page_persists_the_turn_outcome_and_the_queue_acceptance_ordinal() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    let started = MaterializedSessionMutation {
        execution: Some(MaterializedExecutionState::Running { started_at_ms: 90 }),
        active_turn: Some(Some(MaterializedTurn {
            command_id: "prompt-1".into(),
            accepted_ordinal: Some(1),
            turn_start_position: 2,
            started_at_ms: 90,
        })),
        queued_prompts: Some(vec![MaterializedQueuedPrompt {
            command_id: "prompt-2".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "next"})],
            queued_at_ms: 105,
            accepted_ordinal: Some(3),
        }]),
        ..MaterializedSessionMutation::default()
    };
    apply_projection_event_to(
        &database,
        "session-1",
        1,
        RELAY_EVENT_GENESIS_DIGEST,
        &event_digest(1),
        &started,
    )
    .unwrap();

    let reloaded = load_materialized_session_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded
            .active_turn
            .as_ref()
            .map(|turn| turn.accepted_ordinal),
        Some(Some(1))
    );
    assert_eq!(reloaded.queued_prompts[0].accepted_ordinal, Some(3));
    assert_eq!(
        load_materialized_turn_outcome_from(&database, "session-1")
            .unwrap()
            .unwrap()
            .1
            .map(|turn| turn.turn_start_position),
        Some(2)
    );

    let completed = MaterializedSessionMutation {
        execution: Some(MaterializedExecutionState::Idle),
        active_turn: Some(None),
        last_turn_outcome: Some(MaterializedTurnOutcome {
            diagnostic: None,
            usage: None,
            command_id: "prompt-1".into(),
            accepted_ordinal: Some(1),
            turn_start_position: Some(2),
            completed_ordinal: 2,
            completed_at_ms: 400,
            outcome: TurnOutcomeKind::Completed {
                stop_reason: "EndTurn".into(),
            },
        }),
        ..MaterializedSessionMutation::default()
    };
    apply_projection_event_to(
        &database,
        "session-1",
        2,
        &event_digest(1),
        &event_digest(2),
        &completed,
    )
    .unwrap();

    let (execution, active, outcome) = load_materialized_turn_outcome_from(&database, "session-1")
        .unwrap()
        .unwrap();
    assert_eq!(execution, MaterializedExecutionState::Idle);
    assert!(active.is_none(), "a completed turn is no longer running");
    let outcome = outcome.expect("the finished turn's outcome");
    assert_eq!(outcome.accepted_ordinal, Some(1));
    assert_eq!(
        outcome.outcome,
        TurnOutcomeKind::Completed {
            stop_reason: "EndTurn".into()
        }
    );
}

/// The wait endpoint reports a turn number and a final message for the span
/// one turn covers. A message the harness records after the turn ended — a
/// resume notice is one — belongs to no turn and must not displace the answer.
#[test]
fn a_turn_summary_counts_turn_starts_and_reads_that_turn_s_final_message() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    let user = |position: u64, text: &str| TranscriptItem {
        stable_id: format!("user:{position}"),
        position,
        latest_content_event_ordinal: None,
        created_at_ms: position as i64 * 100,
        last_changed_at_ms: position as i64 * 100,
        body: TranscriptBody::User {
            content: vec![serde_json::json!({"type": "text", "text": text})],
        },
    };
    let agent = |position: u64, text: &str| TranscriptItem {
        stable_id: format!("agent:{position}"),
        position,
        latest_content_event_ordinal: Some(position),
        created_at_ms: position as i64 * 100,
        last_changed_at_ms: position as i64 * 100 + 50,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text}
            })],
            streaming: false,
        },
    };
    let items = [
        user(1, "first"),
        agent(2, "first answer"),
        user(3, "second"),
        agent(4, "second answer"),
        // Recorded after the second turn completed, the way a harness reports
        // the model a resumed session opened on.
        agent(5, "Warning: this session was recorded with another model"),
    ];
    for (index, item) in items.into_iter().enumerate() {
        let ordinal = index as u64 + 1;
        let previous = if ordinal == 1 {
            RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            event_digest(ordinal - 1)
        };
        apply_projection_event_to(
            &database,
            "session-1",
            ordinal,
            &previous,
            &event_digest(ordinal),
            &MaterializedSessionMutation {
                transcript: vec![TranscriptMutation::Upsert(item)],
                ..MaterializedSessionMutation::default()
            },
        )
        .unwrap();
    }

    let first = load_materialized_turn_summary_from(&database, "session-1", 1, 2).unwrap();
    assert_eq!(first.turn_number, 1);
    assert_eq!(first.turn_started_at_ms, 100);
    assert_eq!(first.last_changed_at_ms, 250);
    assert_eq!(
        first.final_message.as_deref(),
        Some("first answer"),
        "a turn's summary stops where the turn ended"
    );

    let second = load_materialized_turn_summary_from(&database, "session-1", 3, 4).unwrap();
    assert_eq!(second.turn_number, 2);
    assert_eq!(second.turn_started_at_ms, 300);
    assert_eq!(second.last_changed_at_ms, 450);
    assert_eq!(
        second.final_message.as_deref(),
        Some("second answer"),
        "a message recorded after the turn ended is not the turn's answer"
    );
}

/// A finished child session reports the answer of the turn it ran, so a
/// harness notice recorded after that turn — a resume warning, for instance —
/// must not take its place. The session-wide newest agent message does take
/// its place, which is why the report is read from the turn's own span.
#[test]
fn a_finished_turn_reports_its_own_answer_and_not_a_later_harness_notice() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    let agent = |position: u64, text: &str| TranscriptItem {
        stable_id: format!("agent:{position}"),
        position,
        latest_content_event_ordinal: Some(position),
        created_at_ms: position as i64 * 100,
        last_changed_at_ms: position as i64 * 100,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text}
            })],
            streaming: false,
        },
    };
    let mutations = [
        MaterializedSessionMutation {
            transcript: vec![TranscriptMutation::Upsert(TranscriptItem {
                stable_id: "user:1".into(),
                position: 1,
                latest_content_event_ordinal: None,
                created_at_ms: 100,
                last_changed_at_ms: 100,
                body: TranscriptBody::User {
                    content: vec![serde_json::json!({"type": "text", "text": "do the work"})],
                },
            })],
            execution: Some(MaterializedExecutionState::Running { started_at_ms: 100 }),
            active_turn: Some(Some(MaterializedTurn {
                command_id: "prompt-1".into(),
                accepted_ordinal: Some(1),
                turn_start_position: 1,
                started_at_ms: 100,
            })),
            ..MaterializedSessionMutation::default()
        },
        MaterializedSessionMutation {
            transcript: vec![TranscriptMutation::Upsert(agent(2, "the handoff report"))],
            ..MaterializedSessionMutation::default()
        },
        MaterializedSessionMutation {
            execution: Some(MaterializedExecutionState::Idle),
            active_turn: Some(None),
            last_turn_outcome: Some(MaterializedTurnOutcome {
                diagnostic: None,
                usage: None,
                command_id: "prompt-1".into(),
                accepted_ordinal: Some(1),
                turn_start_position: Some(1),
                completed_ordinal: 3,
                completed_at_ms: 300,
                outcome: TurnOutcomeKind::Completed {
                    stop_reason: "EndTurn".into(),
                },
            }),
            ..MaterializedSessionMutation::default()
        },
        MaterializedSessionMutation {
            transcript: vec![TranscriptMutation::Upsert(agent(
                4,
                "Warning: this session was recorded with another model",
            ))],
            ..MaterializedSessionMutation::default()
        },
    ];
    for (index, mutation) in mutations.into_iter().enumerate() {
        let ordinal = index as u64 + 1;
        let previous = if ordinal == 1 {
            RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            event_digest(ordinal - 1)
        };
        apply_projection_event_to(
            &database,
            "session-1",
            ordinal,
            &previous,
            &event_digest(ordinal),
            &mutation,
        )
        .unwrap();
    }

    assert_eq!(
        load_materialized_finished_turn_message_from(&database, "session-1")
            .unwrap()
            .as_deref(),
        Some("the handoff report")
    );
    assert_eq!(
        load_materialized_session_summary_from(&database, "session-1")
            .unwrap()
            .unwrap()
            .last_agent_message
            .as_deref(),
        Some("Warning: this session was recorded with another model"),
        "the session-wide newest message is the notice, which is what made this worth bounding"
    );
}

#[test]
fn transcript_paging_by_sequence_returns_a_rewritten_agent_message_once() {
    // An agent message is rewritten while it streams. Paging by position would
    // hand a caller the message as first created and never the finished text;
    // paging by sequence sends it again exactly when it changed.
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();

    let mut user = agent_message_mutation(2);
    let TranscriptMutation::Upsert(item) = &mut user.transcript[0] else {
        unreachable!();
    };
    item.stable_id = "user-1".into();
    item.latest_content_event_ordinal = None;
    item.body = TranscriptBody::User {
        content: vec![serde_json::json!({"type": "text", "text": "go"})],
    };

    let mut rewritten = agent_message_mutation(3);
    let TranscriptMutation::Upsert(item) = &mut rewritten.transcript[0] else {
        unreachable!();
    };
    item.stable_id = "item-1".into();
    item.position = 1;

    apply_projection_page_to(&database, "session-1", |page| {
        page.apply(
            1,
            RELAY_EVENT_GENESIS_DIGEST,
            &event_digest(1),
            &agent_message_mutation(1),
        )?;
        page.apply(2, &event_digest(1), &event_digest(2), &user)?;
        page.apply(3, &event_digest(2), &event_digest(3), &rewritten)
    })
    .unwrap();

    let page = load_materialized_transcript_filtered_from(&database, "session-1", 0, 10, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|item| (item.stable_id.as_str(), item.seq()))
            .collect::<Vec<_>>(),
        [("user-1", 2), ("item-1", 3)],
        "the rewritten agent message sorts after the user message it now follows"
    );
    assert_eq!(page.latest_seq, 3);

    let resumed = load_materialized_transcript_filtered_from(&database, "session-1", 2, 10, None)
        .unwrap()
        .unwrap();
    assert_eq!(resumed.items.len(), 1);
    assert_eq!(resumed.items[0].stable_id, "item-1");
    let TranscriptBody::Agent { chunks, .. } = &resumed.items[0].body else {
        panic!("the rewritten item stayed an agent message");
    };
    assert_eq!(
        chunks[0]["content"]["text"], "event 3",
        "a caller resuming from the sequence it saw gets the finished text, once"
    );

    assert!(
        load_materialized_transcript_filtered_from(&database, "unknown", 0, 10, None)
            .unwrap()
            .is_none(),
        "a session with no projection row has no transcript to page"
    );
}

#[test]
fn profile_configuration_cache_survives_reopen_and_expires_or_invalidates() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cache.sqlite3");
    let connection = open(&path).unwrap();
    save_profile_config_cache_with(&connection, "kimi", "model-1", "pin-1", "choices").unwrap();
    drop(connection);
    assert_eq!(
        load_profile_config_cache_from(&path, "kimi", "model-1", "pin-1")
            .unwrap()
            .as_deref(),
        Some("choices")
    );
    assert!(
        load_profile_config_cache_from(&path, "kimi", "model-2", "pin-1")
            .unwrap()
            .is_none()
    );
    assert!(
        load_profile_config_cache_from(&path, "kimi", "model-1", "pin-2")
            .unwrap()
            .is_none()
    );
    open(&path)
        .unwrap()
        .execute("UPDATE profile_config_cache SET observed_at = 0", [])
        .unwrap();
    assert!(
        load_profile_config_cache_from(&path, "kimi", "model-1", "pin-1")
            .unwrap()
            .is_none()
    );
}

#[test]
fn filtered_transcript_pages_include_ties_and_advance_across_gaps() {
    use mj_core::transcript::TranscriptRole;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hel.sqlite3");
    save_session_to(&path, &session("session-1", "project-1")).unwrap();
    apply_projection_page_to(&path, "session-1", |page| {
        for ordinal in 1..=6 {
            let mut mutation = agent_message_mutation(ordinal);
            if ordinal % 2 == 1 {
                let TranscriptMutation::Upsert(item) = &mut mutation.transcript[0] else {
                    unreachable!()
                };
                item.latest_content_event_ordinal = None;
                item.body = TranscriptBody::System { text: "gap".into() };
            }
            if ordinal == 2 {
                let TranscriptMutation::Upsert(item) = &mutation.transcript[0] else {
                    unreachable!()
                };
                let mut tied = item.clone();
                tied.stable_id = "tied-agent".into();
                mutation.transcript.push(TranscriptMutation::Upsert(tied));
            }
            let prior = if ordinal == 1 {
                RELAY_EVENT_GENESIS_DIGEST.into()
            } else {
                event_digest(ordinal - 1)
            };
            page.apply(ordinal, &prior, &event_digest(ordinal), &mutation)?;
        }
        Ok(())
    })
    .unwrap();
    let first = load_materialized_transcript_filtered_from(
        &path,
        "session-1",
        0,
        1,
        Some(TranscriptRole::Agent),
    )
    .unwrap()
    .unwrap();
    assert_eq!(first.items.len(), 2, "a page must not split a sequence tie");
    assert_eq!(first.next_after_seq, 2);
    let second = load_materialized_transcript_filtered_from(
        &path,
        "session-1",
        first.next_after_seq,
        1,
        Some(TranscriptRole::Agent),
    )
    .unwrap()
    .unwrap();
    assert_eq!(second.items[0].seq(), 4);
    let last = load_materialized_transcript_filtered_from(
        &path,
        "session-1",
        4,
        1,
        Some(TranscriptRole::Agent),
    )
    .unwrap()
    .unwrap();
    assert_eq!(last.next_after_seq, 6);
    let empty = load_materialized_transcript_filtered_from(
        &path,
        "session-1",
        0,
        1,
        Some(TranscriptRole::Tool),
    )
    .unwrap()
    .unwrap();
    assert!(empty.items.is_empty());
    assert_eq!(empty.next_after_seq, 6);
}

/// A parent, a child, and the relation between them, written the way the
/// controller writes them.
fn subagent_pair(path: &Path) -> (SessionRecord, SessionRecord) {
    let parent = session("11111111111111111111111111111111", "bundle-1");
    let mut child = session("22222222222222222222222222222222", "bundle-1");
    child.title = "child session".into();
    let mut state = State::default();
    state.sessions.insert(parent.id.clone(), parent.clone());
    state.sessions.insert(child.id.clone(), child.clone());
    state.subagents.insert(
        child.id.clone(),
        mj_core::subagent::SubagentRecord {
            child_session_id: child.id.clone(),
            parent_session_id: parent.id.clone(),
            task_name: "probe".into(),
            profile_id: "codex".into(),
            model: None,
            effort: None,
            working_directory: PathBuf::new(),
            initial_prompt: "say ready".into(),
            request_key: "probe-1".into(),
            created_at: "2026-09-18T00:00:00Z".into(),
            noticed_turn: None,
            handback_tool: false,
        },
    );
    save_state_to(path, &state).unwrap();
    (parent, child)
}

thread_local! {
    static AFTER_STATE_SESSIONS_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

pub(super) fn after_state_sessions_read() {
    let hook = AFTER_STATE_SESSIONS_READ.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[test]
fn state_reads_keep_sessions_and_subagents_in_one_snapshot_during_concurrent_changes() {
    for creating_child in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let (_parent, child) = subagent_pair(&path);
        let mut with_child = load_state_from(&path).unwrap();
        with_child
            .mount_history
            .insert("host".into(), vec![directory.path().join("with-child")]);
        let mut without_child = with_child.clone();
        without_child.sessions.remove(&child.id);
        without_child.subagents.remove(&child.id);
        without_child
            .mount_history
            .insert("host".into(), vec![directory.path().join("without-child")]);
        let (before, after) = if creating_child {
            (without_child, with_child)
        } else {
            (with_child, without_child)
        };
        save_state_to(&path, &before).unwrap();
        let writer_path = path.clone();
        let committed = after.clone();
        AFTER_STATE_SESSIONS_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                // Commit a real creation/deletion between the reader's queries.
                // WAL permits this while a read transaction remains open.
                save_state_to(&writer_path, &committed).unwrap();
                let writer = open(&writer_path).unwrap();
                let violations: i64 = writer
                    .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(violations, 0);
            }));
        });
        let observed = load_state_from(&path).unwrap();
        assert!(AFTER_STATE_SESSIONS_READ.with(|hook| hook.borrow().is_none()));
        assert_eq!(
            observed, before,
            "mixed state while creating_child={creating_child}"
        );
        assert_eq!(load_state_from(&path).unwrap(), after);
    }
}

/// Deleting a child takes its sub-agent relation with it, so this is not how
/// a relation is left behind. Kept as the control for the test below.
#[test]
fn deleting_a_child_session_deletes_its_subagent_relation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hel.sqlite3");
    let (_parent, child) = subagent_pair(&path);

    delete_session_from(&path, &child.id).unwrap();

    let state = load_state_from(&path).unwrap();
    assert!(!state.sessions.contains_key(&child.id));
    assert!(
        state.subagents.is_empty(),
        "the foreign key cascade removes the relation with the child"
    );
}

/// A sub-agent relation with no child session refuses the whole state load,
/// and every later operation fails with it. That is how a spawn came to be
/// answered "sub-agent ... has no child session" long after the child in
/// question was gone (#1065).
///
/// The row is seeded with the foreign key off, because the schema's cascade is
/// what normally prevents it; the point of the test is what the load does when
/// the row exists anyway, whatever left it there.
#[test]
fn a_subagent_relation_with_no_session_does_not_refuse_the_whole_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hel.sqlite3");
    let (parent, child) = subagent_pair(&path);

    let connection = open(&path).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .unwrap();
    connection
        .execute("DELETE FROM sessions WHERE session_id = ?1", [&child.id])
        .unwrap();
    drop(connection);
    assert_eq!(
        count_rows(&path, "subagent_sessions"),
        1,
        "the residue this test is about"
    );

    let state = load_state_from(&path).expect("residue must not refuse the load");

    assert!(state.sessions.contains_key(&parent.id));
    assert!(
        state.subagents.is_empty(),
        "a relation with no child session describes nothing and is dropped"
    );
    state
        .validate()
        .expect("the loaded state must pass its own check");

    // The next save clears the row durably, so the residue does not come back.
    save_state_to(&path, &state).unwrap();
    assert_eq!(count_rows(&path, "subagent_sessions"), 0);
}

fn count_rows(path: &Path, table: &str) -> i64 {
    let connection = open(path).unwrap();
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn workspace_close_discards_all_drafts_but_retains_resumable_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("close.sqlite3");
    let workspace = create_workspace_at(&database, "Close").unwrap();
    let mut record = session("session-close", "project-1");
    record.workspace_id = workspace.id.clone();
    record.draft_input = "unsent legacy input".into();
    save_session_to(&database, &record).unwrap();
    save_detached_draft_at(
        &database,
        &workspace.id,
        Some(&record.id),
        "terminal",
        Some(42),
        "detached input",
    )
    .unwrap();
    close_workspace_at(&database, &workspace.id).unwrap();
    let stored = load_state_from(&database).unwrap();
    assert_eq!(stored.sessions[&record.id].checkpoint, record.checkpoint);
    assert_eq!(stored.sessions[&record.id].state, record.state);
    assert!(stored.sessions[&record.id].draft_input.is_empty());
    assert!(
        list_detached_drafts_at(&database, &workspace.id)
            .unwrap()
            .is_empty()
    );
    assert!(
        !list_workspaces_from(&database)
            .unwrap()
            .iter()
            .any(|w| w.id == workspace.id)
    );
    let destination = create_workspace_at(&database, "Resume here").unwrap();
    reassign_resumable_session_workspace_at(&database, &record.id, &destination.id).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id].workspace_id,
        destination.id
    );
}

#[test]
fn workspace_close_refuses_new_active_sessions_without_discarding_drafts() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("close.sqlite3");
    let workspace = create_workspace_at(&database, "Concurrent create").unwrap();
    let mut record = session("session-new", "project-1");
    record.workspace_id = workspace.id.clone();
    record.state = SessionState::Provisioning;
    record.draft_input = "keep me".into();
    save_session_to(&database, &record).unwrap();
    save_detached_draft_at(
        &database,
        &workspace.id,
        None,
        "terminal",
        Some(42),
        "keep this too",
    )
    .unwrap();
    assert!(close_workspace_at(&database, &workspace.id).is_err());
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id].draft_input,
        "keep me"
    );
    assert_eq!(
        list_detached_drafts_at(&database, &workspace.id)
            .unwrap()
            .len(),
        1
    );
    record.state = SessionState::Stopped;
    save_session_to(&database, &record).unwrap();
    close_workspace_at(&database, &workspace.id).unwrap();
    let mut late = record.clone();
    late.id = "session-too-late".into();
    late.state = SessionState::Provisioning;
    assert!(
        save_session_to(&database, &late).is_err(),
        "creation cannot resurrect a removed workspace"
    );
}

thread_local! {
    static AFTER_MATERIALIZED_FRONTIER_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

pub(super) fn after_materialized_frontier_read() {
    let hook = AFTER_MATERIALIZED_FRONTIER_READ.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[test]
fn projection_reads_keep_one_snapshot_when_a_writer_commits_after_the_frontier_read() {
    for mode in ["whole", "tail", "summary"] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("hel.sqlite3");
        save_session_to(&database, &session("session-1", "project-1")).unwrap();
        let mut before = materialized_session("session-1");
        before.transcript.truncate(2);
        save_materialized_session_to(&database, &before).unwrap();
        let before_summary =
            load_materialized_session_summary_from(&database, "session-1").unwrap();
        let mut after = before.clone();
        after.applied_event_ordinal = 8;
        after.applied_event_digest = event_digest(8);
        let message = Arc::make_mut(&mut after.transcript[1]);
        message.latest_content_event_ordinal = Some(8);
        if let TranscriptBody::Agent { chunks, .. } = &mut message.body {
            chunks
                .push(serde_json::json!({"content":{"type":"text","text":"New streamed content"}}));
        }
        after.transcript.push(Arc::new(TranscriptItem {
            stable_id: "system:8".into(),
            position: 8,
            latest_content_event_ordinal: None,
            created_at_ms: 2_000,
            last_changed_at_ms: 2_000,
            body: TranscriptBody::System {
                text: "New publication".into(),
            },
        }));
        let writer_path = database.clone();
        AFTER_MATERIALIZED_FRONTIER_READ.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                // A separate WAL connection commits between the reader's queries.
                // This must succeed without waiting for the reader to finish.
                save_materialized_session_to(&writer_path, &after).unwrap();
            }));
        });
        match mode {
            "whole" => assert_eq!(
                load_materialized_session_from(&database, "session-1").unwrap(),
                Some(before)
            ),
            "tail" => {
                let (actual, window) =
                    load_materialized_projection_tail_from(&database, "session-1", 1)
                        .unwrap()
                        .unwrap();
                assert_eq!(window.omitted_items, 1);
                before.transcript.remove(0);
                assert_eq!(actual, before);
            }
            _ => assert_eq!(
                load_materialized_session_summary_from(&database, "session-1").unwrap(),
                before_summary
            ),
        }
        assert!(AFTER_MATERIALIZED_FRONTIER_READ.with(|hook| hook.borrow().is_none()));
        assert_eq!(
            load_materialized_session_from(&database, "session-1")
                .unwrap()
                .unwrap()
                .applied_event_ordinal,
            8
        );
    }
}

#[test]
fn continuation_reads_earlier_authorization_outside_the_ui_window_at_one_frontier() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("continuation.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut materialized = materialized_session("session-1");
    materialized.transcript = vec![
        Arc::new(TranscriptItem {
            stable_id: "user:request".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 1,
            last_changed_at_ms: 1,
            body: TranscriptBody::User {
                content: vec![
                    serde_json::json!({"type":"text","text":"Implement the parser and run tests"}),
                ],
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "system:notice".into(),
            position: 2,
            latest_content_event_ordinal: None,
            created_at_ms: 2,
            last_changed_at_ms: 2,
            body: TranscriptBody::System {
                text: "not authorization".into(),
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "agent:reply".into(),
            position: 3,
            latest_content_event_ordinal: Some(3),
            created_at_ms: 3,
            last_changed_at_ms: 3,
            body: TranscriptBody::Agent {
                chunks: vec![
                    serde_json::json!({"content":{"type":"text","text":"Implemented. Shall I test?"}}),
                ],
                streaming: false,
            },
        }),
    ];
    save_materialized_session_to(&database, &materialized).unwrap();
    let (window, metadata) = load_materialized_projection_tail_from(&database, "session-1", 1)
        .unwrap()
        .unwrap();
    assert!(metadata.omitted_items > 0);
    assert!(crate::continuation::evidence(&window).is_err());
    let collected = load_continuation_evidence_from(
        &database,
        "session-1",
        materialized.applied_event_ordinal,
        &materialized.applied_event_digest,
    )
    .unwrap();
    assert_eq!(
        collected,
        crate::continuation::evidence(&materialized).unwrap()
    );
    assert_eq!(
        collected.messages[0].text,
        "Implement the parser and run tests"
    );
    assert!(
        load_continuation_evidence_from(
            &database,
            "session-1",
            materialized.applied_event_ordinal + 1,
            &materialized.applied_event_digest
        )
        .is_err()
    );
    assert!(
        load_continuation_evidence_from(
            &database,
            "session-1",
            materialized.applied_event_ordinal,
            "wrong-digest"
        )
        .is_err()
    );
}

#[test]
fn quota_recovery_migration_advances_the_breaking_floor_and_preserves_cache() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("isolated-quota-migration.sqlite3");
    drop(open(&path).unwrap());
    forget_verified_schema(&path);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(
        "DROP TABLE quota_reset_cache;
        DELETE FROM schema_migrations WHERE version >= 44;
        UPDATE schema_compatibility SET minimum_compatible_version = 43;
        PRAGMA user_version = 43;",
    )
    .unwrap();
    drop(raw);
    let connection = open(&path).unwrap();
    let state = schema::read_schema_state(&connection).unwrap();
    assert_eq!(state.revision, SCHEMA_VERSION);
    let floor: i64 = connection
        .query_row(
            "SELECT minimum_compatible_version FROM schema_compatibility",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(floor >= 44, "older JSON readers must be refused");
    connection
        .execute("INSERT INTO quota_reset_cache VALUES ('account', '{}')", [])
        .unwrap();
    drop(connection);
    forget_verified_schema(&path);
    let connection = open(&path).unwrap();
    let body: String = connection
        .query_row(
            "SELECT body FROM quota_reset_cache WHERE identity='account'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(body, "{}");
}

/// A deferred transaction that has already read cannot wait for the WAL write
/// lock: SQLite only calls the busy handler when the connection holds no
/// transaction, so the upgrade returns `SQLITE_BUSY` at once. Writer-capable
/// connections therefore begin IMMEDIATE (issue 1117).
#[test]
fn writer_connections_wait_for_a_concurrent_writer_instead_of_failing() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let mut recorder = open(&database).unwrap();
    let mut holder = open(&database).unwrap();
    let (holding_tx, holding) = sync_channel(1);
    let hold = thread::spawn(move || {
        let transaction = holder
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        holding_tx.send(()).unwrap();
        // Well under the writer's five-second busy timeout.
        thread::sleep(Duration::from_millis(300));
        transaction.commit().unwrap();
    });
    holding.recv().unwrap();

    let activity = ApiActivityState {
        state: "running".into(),
        details: None,
        is_idle: false,
        waiting_for_input: false,
        capacity_retry: false,
    };
    record_api_activities_with(
        &mut recorder,
        vec![("session-1".into(), activity.clone())],
        1_000,
    )
    .unwrap();
    hold.join().unwrap();

    let body: String = recorder
        .query_row(
            "SELECT body FROM api_session_activity WHERE session_id = ?1",
            ["session-1"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<ApiActivityState>(&body).unwrap(),
        activity
    );
}

/// The `cfg(test)` `open_reader` alias must stay DEFERRED: fixture reads may
/// not take the write lock, or a reader would block the writer under test.
#[test]
fn test_reader_connections_keep_deferred_transactions() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hel.sqlite3");
    save_session_to(&database, &session("session-1", "project-1")).unwrap();
    let reader = open_reader(&database).unwrap();
    let snapshot = reader.unchecked_transaction().unwrap();
    let sessions: i64 = snapshot
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sessions, 1);

    let mut writer = open(&database).unwrap();
    let write = writer.transaction().unwrap();
    write
        .execute("DELETE FROM api_session_activity", [])
        .unwrap();
    write.commit().unwrap();
    drop(snapshot);
}

fn long_conversation() -> MaterializedSession {
    let mut projection = MaterializedSession::empty("history-window");
    projection.applied_event_ordinal = 2500;
    projection.applied_event_digest = event_digest(2500);
    projection.transcript = (1..=2500).map(|position| Arc::new(TranscriptItem {
        stable_id: format!("{}:{position:04}", if position % 100 == 1 { "user" } else { "agent" }), position,
        latest_content_event_ordinal: (position % 100 != 1).then_some(position), created_at_ms: position as i64,
        last_changed_at_ms: position as i64,
        body: if position % 100 == 1 {
            TranscriptBody::User { content: vec![serde_json::json!({"type":"text", "text":format!("request {position}")})] }
        } else {
            TranscriptBody::Agent { chunks: vec![serde_json::json!({"content":{"type":"text", "text":"answer ".repeat(50)}})], streaming: false }
        },
    })).collect();
    projection
}

#[test]
fn actor_history_window_matches_live_trimming_and_preserves_pending_turns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let mut full = long_conversation();
    save_session_to(&path, &session(&full.session_id, "project-1")).unwrap();
    for pending in [false, true] {
        if let TranscriptBody::Agent { streaming, .. } =
            &mut Arc::make_mut(&mut full.transcript[1]).body
        {
            *streaming = pending;
        }
        save_materialized_session_to(&path, &full).unwrap();
        let (loaded, window) = load_materialized_actor_projection_from(&path, &full.session_id)
            .unwrap()
            .unwrap();
        let mut trimmed = full.clone();
        let mut expected = ProjectionWindow::of(&full);
        expected.trim(&mut trimmed, PROJECTION_TAIL_ITEMS);
        assert_eq!(loaded, trimmed);
        assert_eq!(window, expected);
        assert_eq!(loaded.transcript.len(), if pending { 2500 } else { 1100 });
        assert_eq!(window.provisional_title.as_deref(), Some("request 1"));
        assert_eq!(
            load_materialized_session_from(&path, &full.session_id)
                .unwrap()
                .unwrap(),
            full,
            "the durable source for checkpoints must stay complete"
        );
    }
}

#[test]
fn history_pages_have_no_gaps_across_position_ties_and_concurrent_appends() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let mut full = long_conversation();
    // Streaming content revisions do not change a message's history position.
    for item in &mut full.transcript {
        let item = Arc::make_mut(item);
        item.position = item.position.div_ceil(2);
        if matches!(item.body, TranscriptBody::Agent { .. }) {
            item.latest_content_event_ordinal = Some(2500);
        }
    }
    full.transcript
        .sort_by(|a, b| (a.position, &a.stable_id).cmp(&(b.position, &b.stable_id)));
    save_session_to(&path, &session(&full.session_id, "project-1")).unwrap();
    save_materialized_session_to(&path, &full).unwrap();
    let mut before = None;
    let mut pages = Vec::new();
    loop {
        let page = load_transcript_history_from(&path, &full.session_id, before.as_ref(), 127)
            .unwrap()
            .unwrap();
        assert!(page.items.len() <= 127);
        assert!(
            page.items
                .windows(2)
                .all(|pair| (pair[0].position, &pair[0].stable_id)
                    < (pair[1].position, &pair[1].stable_id))
        );
        pages.push(page.items);
        before = page.before;
        if pages.len() == 1 {
            let mut appended = full.clone();
            let mut item = (*full.transcript.last().unwrap().clone()).clone();
            item.stable_id = "new-message".into();
            item.position = 2501;
            item.latest_content_event_ordinal = Some(2501);
            appended.transcript.push(Arc::new(item));
            appended.applied_event_ordinal = 2501;
            appended.applied_event_digest = event_digest(2501);
            save_materialized_session_to(&path, &appended).unwrap();
        }
        if before.is_none() {
            break;
        }
    }
    let read: Vec<_> = pages.into_iter().rev().flatten().collect();
    assert_eq!(read, full.transcript);
    assert_eq!(
        load_transcript_history_from(&path, &full.session_id, None, usize::MAX)
            .unwrap()
            .unwrap()
            .items
            .len(),
        256
    );
    assert!(
        load_transcript_history_from(&path, "missing", None, 10)
            .unwrap()
            .is_none()
    );
}

#[test]
fn history_window_rehydrates_late_tool_updates_before_durable_projection() {
    use agent_client_protocol::schema::v1::{
        SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };
    use mj_core::relay::{RELAY_EVENT_FORMAT_V1, RelayEvent, RelayObservation};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let mut full = long_conversation();
    let old_tool = materialized_session(&full.session_id).transcript[2].clone();
    full.transcript[2] = old_tool.clone();
    save_session_to(&path, &session(&full.session_id, "project-1")).unwrap();
    save_materialized_session_to(&path, &full).unwrap();
    let (mut live, mut window) = load_materialized_actor_projection_from(&path, &full.session_id)
        .unwrap()
        .unwrap();
    assert!(
        !live
            .transcript
            .iter()
            .any(|item| item.stable_id == old_tool.stable_id)
    );
    let mut event = RelayEvent {
        format: RELAY_EVENT_FORMAT_V1,
        ordinal: 2501,
        previous_digest: full.applied_event_digest.clone(),
        digest: String::new(),
        recorded_at_ms: 5000,
        command_id: None,
        observation: RelayObservation::SessionUpdate {
            update: Box::new(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .title("Late result"),
            ))),
        },
    };
    event.digest = mj_core::relay::relay_event_digest(&event).unwrap();
    let restored =
        load_projection_references_from(&path, &live, std::slice::from_ref(&event)).unwrap();
    assert_eq!(restored, vec![old_tool.clone()]);
    window.omitted_items -= restored.len();
    live.transcript.splice(0..0, restored);
    assert!(
        load_projection_references_from(&path, &live, std::slice::from_ref(&event))
            .unwrap()
            .is_empty()
    );
    let mutation = mj_transcript::projection::project_relay_event(&live, &event)
        .unwrap()
        .mutation;
    apply_projection_page_to(&path, &full.session_id, |page| {
        page.apply(
            event.ordinal,
            &event.previous_digest,
            &event.digest,
            &mutation,
        )
    })
    .unwrap();
    mj_transcript::projection::apply_committed_projection_event(&mut live, &event, mutation)
        .unwrap();
    window.trim(&mut live, PROJECTION_TAIL_ITEMS);
    let saved = load_materialized_session_from(&path, &full.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(saved.transcript.len(), 2500);
    let tool = saved
        .transcript
        .iter()
        .find(|item| item.stable_id == old_tool.stable_id)
        .unwrap();
    assert_eq!(tool.position, old_tool.position);
    assert_eq!(tool.created_at_ms, old_tool.created_at_ms);
    let TranscriptBody::Tool { call, .. } = &tool.body else {
        panic!("tool remains a tool")
    };
    assert_eq!(call["title"], "Late result");
    assert_eq!(
        live.transcript.len() + window.omitted_items,
        saved.transcript.len()
    );
}

#[test]
fn target_access_survives_lifecycle_updates_and_changes_with_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("target-runtime.sqlite3");
    let mut record = session("runtime-session", "project");
    let template: mj_core::config::TargetTemplate = serde_json::from_str(
        r#"{"kind":"ssh-podman","host":"original.test","user":"builder","identity_file":"/keys/id","extra_args":["-p","2222"],"image":"test"}"#
    ).unwrap();
    record.target_runtime = Some((&template).into());
    record.target = Some(TargetLocator::SshPodman {
        host: "original.test".into(),
        container_id: "original-container".into(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    });
    save_session_to(&database, &record).unwrap();
    record.state = SessionState::Error;
    save_lifecycle_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id],
        record
    );
    record.target_runtime = Some((&mj_core::config::TargetTemplate::LocalBare).into());
    record.target = Some(TargetLocator::LocalBare {
        worker_root: PathBuf::from("/new").join(&record.id),
    });
    save_lifecycle_session_to(&database, &record).unwrap();
    assert_eq!(
        load_state_from(&database).unwrap().sessions[&record.id],
        record
    );
}

/// A child's report is recorded once per turn and replaced by the next turn's,
/// and it goes away with the child's relation.
#[test]
fn a_subagent_report_is_kept_once_per_turn_and_leaves_with_its_child() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let (_parent, child) = subagent_pair(&path);
    let handback = |command_id: &str, message: &str| mj_core::subagent::SubagentHandback {
        command_id: command_id.into(),
        message: message.into(),
        recorded_at_ms: 1,
    };
    assert_eq!(
        load_subagent_report_from(&path, &child.id).unwrap(),
        mj_core::subagent::SubagentReport::default()
    );
    assert!(record_subagent_handback_to(&path, &child.id, &handback("turn-1", "first")).unwrap());
    assert!(
        !record_subagent_handback_to(&path, &child.id, &handback("turn-1", "again")).unwrap(),
        "a second report for the same turn is refused"
    );
    assert!(record_subagent_handback_to(&path, &child.id, &handback("turn-2", "next")).unwrap());
    assert_eq!(
        load_subagent_report_from(&path, &child.id)
            .unwrap()
            .handback,
        Some(handback("turn-2", "next"))
    );

    let mut state = load_state_from(&path).unwrap();
    state.subagents.remove(&child.id);
    state.sessions.remove(&child.id);
    save_state_to(&path, &state).unwrap();
    assert_eq!(
        load_subagent_report_from(&path, &child.id).unwrap(),
        mj_core::subagent::SubagentReport::default(),
        "the report leaves with its child"
    );
}

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
// `anyhow::Error::context` is inherent; only the Unix-only `Option`/`Result`
// calls need the trait in scope.
#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;

#[cfg(unix)]
use crate::controller::restore_session_after_persistence_failure;
use crate::controller::test_support::IsolatedTest;
use crate::controller::test_support::{
    RefusingExecutor, checkpoint_test_session, write_checkpoint_gate_archive,
};
#[cfg(unix)]
use crate::session_manager::{ManagedSessionHandle, new_command_id};
#[cfg(unix)]
use crate::targets::ProcessExecutor;
use crate::worker_client::RelayTransportDead;
use mj_checkpoint::archive::{
    BundleManifest, CanonicalTranscriptBody, CanonicalTranscriptItem, TargetManifest,
};
use mj_checkpoint::checkpoint::CheckpointExportSpec;
#[cfg(unix)]
use mj_core::config::{Config, HarnessProfile, ProjectBundle, ProjectRepository, TargetTemplate};
#[cfg(unix)]
use mj_core::state::TargetLocator;
use mj_core::state::{
    CheckpointMetadata, ManagedSessionSnapshot, MaterializedSession, SessionState, State,
};
use mj_transcript::projection::canonical_session_from_materialized_owned as canonical_session_from_materialized;

#[cfg(unix)]
use crate::targets::ProvisionStage;
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
#[cfg(unix)]
use mj_core::relay::RelayCommand;
use mj_core::relay::{RelayCursor, RelayExecutionState};

pub(crate) use super::stand_in::*;
use super::*;

/// Build the layout for a managed-worktree session whose worktree holds one
/// commit of its own, and return the capture along with the commits it has
/// to distinguish.
fn managed_worktree_export_capture(
    clear_recorded_base: bool,
) -> (CheckpointRepositoryCapture, String, String) {
    let session_id = "2123456789abcdef0123456789abcdef";
    let repository = crate::controller::test_support::committed_repository();
    let mut session =
        crate::controller::test_support::managed_worktree_session(repository.path(), session_id);
    let creation_commit =
        crate::controller::test_support::test_git(repository.path(), &["rev-parse", "HEAD"]);
    if clear_recorded_base {
        session.managed_worktree.as_mut().unwrap().base_commit = None;
    }

    let worktree_root = session
        .managed_worktree
        .as_ref()
        .unwrap()
        .worktree_root
        .clone();
    std::fs::write(worktree_root.join("session.txt"), "work\n").unwrap();
    crate::controller::test_support::test_git(&worktree_root, &["add", "."]);
    crate::controller::test_support::test_git(&worktree_root, &["commit", "-m", "session work"]);
    let worktree_head =
        crate::controller::test_support::test_git(&worktree_root, &["rev-parse", "HEAD"]);

    session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/home/dev/.local/share/hel/workers/session"),
    }));
    let mut state = State::default();
    state.sessions.insert(session_id.into(), session);
    state
        .environment_owners
        .insert(session_id.into(), session_id.into());
    let controller = Controller {
        config: crate::controller::test_support::resume_compatibility_config(),
        state,
    };

    let mut layout = controller
        .session_export_layout(session_id, &targets::ProcessExecutor)
        .unwrap();
    (
        layout.repositories.remove(0).capture,
        creation_commit,
        worktree_head,
    )
}

/// A bare local session may run in a plain folder. Its checkpoint records the
/// directory but claims no Git state, so a restore can never rewrite or check
/// out the user's own files there.
#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a bare project directory runs mkdir and requires POSIX paths"
)]
fn a_plain_project_directory_is_captured_without_git() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("todo.md"), "# todo\n").unwrap();
    // Accepting the folder is what records it as plain; the layout reads that
    // record rather than asking Git again.
    let resolved =
        mj_core::repository::resolve_local_directory(directory.path(), &targets::ProcessExecutor)
            .unwrap();
    assert!(resolved.plain_directory);
    let project = crate::project_catalog::directory_snapshot(&resolved);
    assert!(
        project
            .plain_directories
            .contains(&project.bundle.primary_repo),
        "accepting a plain folder records it as one"
    );
    let mut plain = crate::controller::test_support::checkpoint_test_session(
        "3123456789abcdef0123456789abcdef",
    );
    let session_id = plain.id.clone();
    plain.state = SessionState::Stopped;
    plain.target_template_id = "local-bare".into();
    plain.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/home/dev/.local/share/hel/workers/session"),
    }));
    plain.project_directory = Some(directory.path().to_path_buf());
    let mut recorded = project.clone();
    recorded
        .plain_directories
        .insert(recorded.bundle.primary_repo.clone());
    plain.project = Some(recorded.into());
    let mut state = State::default();
    state.sessions.insert(session_id.clone(), plain);
    state
        .environment_owners
        .insert(session_id.clone(), session_id.clone());
    let controller = Controller {
        config: crate::controller::test_support::resume_compatibility_config(),
        state,
    };

    let mut layout = controller
        .session_export_layout(&session_id, &targets::ProcessExecutor)
        .unwrap();
    let repository = layout.repositories.remove(0);
    assert!(matches!(
        repository.capture,
        CheckpointRepositoryCapture::PlainDirectory
    ));
    assert_eq!(
        repository.relative_destination,
        PathBuf::from(directory.path().file_name().unwrap())
    );
    assert!(
        directory.path().join("todo.md").is_file(),
        "reading the layout leaves the folder alone"
    );
}

// Hard-won: 074faa0c0155: managed worktree checkpoints omitted every commit after using the live HEAD as base
#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn a_managed_worktree_checkpoint_bundles_from_the_recorded_base() {
    let (capture, creation_commit, worktree_head) = managed_worktree_export_capture(false);
    let CheckpointRepositoryCapture::DeltaFrom { base_commit } = capture else {
        panic!("a managed worktree must be captured as a delta, got {capture:?}");
    };
    assert_eq!(base_commit, creation_commit);
    assert_ne!(base_commit, worktree_head);
}

// Hard-won: 074faa0c0155: legacy managed worktree checkpoints omitted commits by using the live HEAD as base
#[test]
#[cfg_attr(
    windows,
    ignore = "#1292: a local managed checkout runs mkdir and requires POSIX paths"
)]
fn a_managed_worktree_without_a_recorded_base_uses_its_branch_creation_commit() {
    let (capture, creation_commit, worktree_head) = managed_worktree_export_capture(true);
    let CheckpointRepositoryCapture::DeltaFrom { base_commit } = capture else {
        panic!("a managed worktree must be captured as a delta, got {capture:?}");
    };
    assert_eq!(base_commit, creation_commit);
    assert_ne!(base_commit, worktree_head);
}

#[test]
fn startup_reconciliation_only_removes_unreferenced_controller_checkpoints() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "1123456789abcdef0123456789abcdef";
    let referenced_name =
        format!("{session_id}-7-archive-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.hel.zip");
    let orphan_name = format!("{session_id}-8-archive-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.hel.zip");
    let imported_name = format!("{session_id}.hel.zip");
    for name in [
        &referenced_name,
        &orphan_name,
        &imported_name,
        "notes.hel.zip",
    ] {
        std::fs::write(directory.path().join(name), b"test").unwrap();
    }
    let checkpoint = CheckpointMetadata {
        archive_path: directory.path().join(&referenced_name),
        sha256: "c".repeat(64),
        created_at: "2026-08-12T00:00:00Z".into(),
        event_frontier: 7,
    };

    assert_eq!(
        reconcile_managed_checkpoint_archives_in(directory.path(), [&checkpoint], &[], &[], &[])
            .unwrap(),
        1
    );
    assert!(directory.path().join(referenced_name).exists());
    assert!(!directory.path().join(orphan_name).exists());
    assert!(directory.path().join(imported_name).exists());
    assert!(directory.path().join("notes.hel.zip").exists());
}
/// A snapshot of a session whose checkpoint barrier is open but not yet
/// ready, projected exactly at `cursor`.
fn checkpoint_barrier_snapshot(cursor: &RelayCursor) -> ManagedSessionSnapshot {
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = cursor.ordinal;
    materialized.applied_event_digest = cursor.digest.clone();
    ManagedSessionSnapshot {
        subagent_requests: Default::default(),
        subagent_completions: Default::default(),
        subagent_queue_error: None,
        window: mj_core::state::ProjectionWindow::of(&materialized),
        materialized,
        latest_credential_sync_signal: None,
        worker_build: None,
        operational: mj_core::relay::RelayOperationalState {
            in_place_move_hold: None,
            assessment: None,
            open_request: None,
            notification_stack: Default::default(),
            notifications_read_through: 0,
            person_input_through: 0,
            work_liveness: None,
            undelivered_mailbox_event_keys: Vec::new(),
            assessment_context: None,
            turn_completion: None,
            continuation: Default::default(),
            relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
            native_agents: Vec::new(),
            steering: None,
            cancelling_prompt_id: None,
            clear_context: false,
            clear_context_started_at_ms: None,
            native_agent_count: 0,
            expected_continuation: None,
            inferred_idle_since_ms: None,
            task_settled_at_ms: None,
            background_needed: None,
            goal: serde_json::from_value(
                serde_json::json!({"known":true,"execution":{"version":1,"status":"idle"}}),
            )
            .unwrap(),
            capacity_retry: None,
            retry_assessment_pending: false,
            authentication_retry_waiting: None,
            jev_rate_limit: None,
            held_login_fingerprint: None,
            authentication_retry_intent: None,
            activity_turn_started_at_ms: None,
            checkpoint_only: false,
            acp_ready: None,
            store_id: None,
            idle_since_ms: None,
            session_id: "session-1".into(),
            execution: RelayExecutionState::Idle,
            latest_ordinal: cursor.ordinal,
            latest_digest: cursor.digest.clone(),
            acknowledged_through: cursor.ordinal,
            acknowledged_digest: cursor.digest.clone(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
            native_session_id: Some("native-session".into()),
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
            agent_capabilities: None,
            agent_info: None,
            runtime: None,
            steering_supported: None,
            mailbox_delivery: None,
            turn_controls: None,
            config_options: Vec::new().into(),
            modes: None,
            available_commands: Vec::new().into(),
            config: BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: Some("checkpoint-1".into()),
            checkpoint_ready: None,
            harness_preparation: None,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            tools_in_flight: Vec::new(),
            activity: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
            background_work_known: None,
        }
        .into(),
    }
}
#[test]
fn checkpoint_barrier_is_not_reached_until_its_ready_cursor_is_projected() {
    let cursor = RelayCursor {
        ordinal: 7,
        digest: "a".repeat(64),
    };
    let mut snapshot = checkpoint_barrier_snapshot(&cursor);

    assert!(!checkpoint_barrier_is_ready(&snapshot, "checkpoint-1"));
    snapshot.operational.checkpoint_ready = Some(cursor.clone());
    assert!(checkpoint_barrier_is_ready(&snapshot, "checkpoint-1"));
    validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).unwrap();
}
#[test]
fn checkpoint_revalidation_accepts_a_frontier_that_moved_past_the_ready_cursor() {
    let cursor = RelayCursor {
        ordinal: 7,
        digest: "a".repeat(64),
    };
    let mut snapshot = checkpoint_barrier_snapshot(&cursor);
    snapshot.operational.checkpoint_ready = Some(cursor.clone());

    // An open ordinary barrier keeps accepting and journalling commands; it
    // only freezes dispatch. The archive still matches the sealed
    // workspace, so a frontier past the ready cursor stays valid.
    snapshot.operational.latest_ordinal = cursor.ordinal + 2;
    snapshot.operational.latest_digest = "b".repeat(64);
    snapshot.materialized.applied_event_ordinal = cursor.ordinal + 2;
    snapshot.materialized.applied_event_digest = "b".repeat(64);
    validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).unwrap();

    // Losing the barrier, or reaching a different cut, still invalidates it.
    snapshot.operational.checkpoint_ready = Some(RelayCursor {
        ordinal: cursor.ordinal + 1,
        digest: "c".repeat(64),
    });
    assert!(validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).is_err());
    snapshot.operational.checkpoint_ready = Some(cursor.clone());
    snapshot.operational.checkpoint_barrier = None;
    assert!(validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor).is_err());
}

#[test]
fn routine_kimi_checkpoint_defers_when_background_liveness_is_not_safe() {
    let cursor = RelayCursor {
        ordinal: 7,
        digest: "a".repeat(64),
    };
    let mut snapshot = checkpoint_barrier_snapshot(&cursor);
    snapshot.operational.checkpoint_ready = Some(cursor.clone());

    for (known, has_task, label) in [
        (Some(false), false, "tracker reported a failure"),
        (Some(true), true, "a native task is still active"),
        (None, false, "an older worker omitted the tracker field"),
    ] {
        snapshot.operational.background_work_known = known;
        snapshot.operational.background_commands = has_task
            .then(|| mj_core::relay::BackgroundCommand {
                id: "kimi:agent-1".into(),
                started_at_ms: 1,
                command: "background agent".into(),
                can_stop: false,
            })
            .into_iter()
            .collect();
        let error = validate_automatic_checkpoint_barrier_snapshot(
            &snapshot,
            "checkpoint-1",
            &cursor,
            HarnessKind::Kimi,
        )
        .expect_err(label);
        assert!(checkpoint_was_deferred(&error), "{label}: {error:#}");
        assert!(!checkpoint_barrier_needs_worker_restart(&error));
    }

    // A non-Kimi harness keeps the historical behavior even if the
    // optional Kimi field happens to be absent or a provider-level list is
    // present in a compatibility snapshot.
    snapshot.operational.background_work_known = None;
    snapshot.operational.background_commands = vec![mj_core::relay::BackgroundCommand {
        id: "legacy-task".into(),
        started_at_ms: 1,
        command: "legacy background work".into(),
        can_stop: false,
    }];
    validate_automatic_checkpoint_barrier_snapshot(
        &snapshot,
        "checkpoint-1",
        &cursor,
        HarnessKind::Codex,
    )
    .expect("non-Kimi checkpoint compatibility");
}
/// Target-side answer of a successful export.
fn exported_checkpoint_json() -> Vec<u8> {
    serde_json::to_vec(&mj_checkpoint::checkpoint::TargetCheckpoint {
        path: PathBuf::from("/var/lib/hel/workers/session/checkpoint.hel.zip"),
        sha256: "c".repeat(64),
        event_frontier: 7,
        event_frontier_digest: "d".repeat(64),
        timings: None,
        manifest: None,
    })
    .unwrap()
}
fn export_spec_fixture() -> CheckpointExportSpec {
    CheckpointExportSpec {
        consent: Default::default(),
        report_directories: None,
        protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
        session: mj_checkpoint::archive::SessionManifest {
            id: LATCH_RELAY_SESSION.into(),
            title: "streamed spec".into(),
            harness_kind: mj_core::config::HarnessKind::Codex,
            profile_id: "codex".into(),
            native_session_id: "native-session".into(),
            created_at: "2026-08-12T00:00:00Z".into(),
            checkpointed_at: "2026-08-16T00:00:00Z".into(),
            hel_version: "test".into(),
            relay_version: "test".into(),
            adapter_version: "acp-v1".into(),
        },
        target: TargetManifest {
            template_id: "podman".into(),
            target_kind: "local-podman".into(),
            details: BTreeMap::new(),
        },
        bundle: BundleManifest {
            id: "project".into(),
            primary_repository: "app".into(),
        },
        relay_root: PathBuf::from("/var/lib/hel/workers/session"),
        harness_home: PathBuf::from("/var/lib/hel/profiles/codex"),
        workspace_root: PathBuf::from("/workspace"),
        repositories: Vec::new(),
        canonical_session: canonical_session_from_materialized(MaterializedSession::empty(
            LATCH_RELAY_SESSION.to_owned(),
        ))
        .unwrap(),
        output_path: PathBuf::from("/var/lib/hel/workers/session/checkpoint.hel.zip"),
    }
}
/// Answers the streamed export with a scripted status, and every other
/// command as a success.
struct ExportExecutor {
    streamed_status: i32,
    streamed_stderr: String,
    retry_stdin_after_failure: bool,
    stdin_calls: Cell<usize>,
    purposes: RefCell<Vec<String>>,
    streamed_spec: RefCell<Vec<u8>>,
}
impl ExportExecutor {
    fn new(streamed_status: i32, streamed_stderr: &str) -> Self {
        Self {
            streamed_status,
            streamed_stderr: streamed_stderr.to_owned(),
            retry_stdin_after_failure: false,
            stdin_calls: Cell::new(0),
            purposes: RefCell::new(Vec::new()),
            streamed_spec: RefCell::new(Vec::new()),
        }
    }

    fn retry_stdin_after_failure(mut self) -> Self {
        self.retry_stdin_after_failure = true;
        self
    }
}
impl CommandExecutor for ExportExecutor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.purposes.borrow_mut().push(command.purpose.clone());
        Ok(CommandOutput {
            status: 0,
            stdout: exported_checkpoint_json(),
            stderr: Vec::new(),
        })
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        self.purposes.borrow_mut().push(command.purpose.clone());
        let mut spec = Vec::new();
        input.read_to_end(&mut spec)?;
        *self.streamed_spec.borrow_mut() = spec;
        let attempt = self.stdin_calls.get();
        self.stdin_calls.set(attempt + 1);
        let failed = self.streamed_status != 0 && (attempt == 0 || !self.retry_stdin_after_failure);
        Ok(CommandOutput {
            status: if failed { self.streamed_status } else { 0 },
            stdout: if failed {
                Vec::new()
            } else {
                exported_checkpoint_json()
            },
            stderr: if failed {
                self.streamed_stderr.clone().into_bytes()
            } else {
                Vec::new()
            },
        })
    }
}

/// The spec reaches the worker as the same JSON `serde_json::to_vec` gives,
/// although it is serialized while the command reads; a borrowed pack spec
/// matches the owned one. The transcript is far larger than a pipe buffer and
/// the stream's chunks.
#[test]
fn a_streamed_spec_is_the_same_json_as_the_whole_spec() {
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        workspace_storage: Default::default(),
    };
    let mut spec = export_spec_fixture();
    spec.canonical_session.transcript = (0..40)
        .map(|index| mj_core::archive::CanonicalTranscriptItem {
            stable_id: format!("item-{index}"),
            position: index,
            latest_content_event_ordinal: None,
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: mj_core::archive::CanonicalTranscriptBody::System {
                text: format!("{index}: {}", "é\"line\n".repeat(20_000)),
            },
        })
        .collect();
    // serde_json writes an unescaped run in one call; it must cross chunks intact.
    spec.canonical_session
        .transcript
        .push(mj_core::archive::CanonicalTranscriptItem {
            stable_id: "item-unescaped".into(),
            position: 40,
            latest_content_event_ordinal: None,
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: mj_core::archive::CanonicalTranscriptBody::System {
                text: "a".repeat(1024 * 1024 + 7),
            },
        });
    let expected = serde_json::to_vec(&spec).unwrap();
    assert!(expected.len() > 4 * 1024 * 1024);
    let executor = ExportExecutor::new(0, "");
    run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &spec,
        export_stdin_command,
        "export target checkpoint",
        None,
    )
    .unwrap();
    assert!(executor.streamed_spec.into_inner() == expected);

    let owned = mj_checkpoint::checkpoint::CheckpointPackSpec {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        relay_root: spec.relay_root.clone(),
        stage_path: PathBuf::from("/stage"),
        canonical_session: spec.canonical_session.clone(),
        output_path: spec.output_path.clone(),
    };
    let stage_path = PathBuf::from("/stage");
    let borrowed = CheckpointPackSpecRef {
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        relay_root: &spec.relay_root,
        stage_path: &stage_path,
        canonical_session: &spec.canonical_session,
        output_path: &spec.output_path,
    };
    let executor = ExportExecutor::new(0, "");
    run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &borrowed,
        pack_stdin_command,
        "pack target checkpoint",
        None,
    )
    .unwrap();
    assert!(executor.streamed_spec.into_inner() == serde_json::to_vec(&owned).unwrap());
}

#[test]
fn a_failing_export_is_not_retried_as_an_old_worker() {
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        workspace_storage: Default::default(),
    };
    let executor = ExportExecutor::new(1, "Error: repository 'app' is missing\n");

    let error = run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &export_spec_fixture(),
        export_stdin_command,
        "export target checkpoint",
        None,
    )
    .unwrap_err();

    assert!(
        format!("{error:#}").contains("repository 'app' is missing"),
        "{error:#}"
    );
    assert_eq!(
        executor.purposes.into_inner(),
        vec!["export target checkpoint".to_owned()]
    );
}
/// R4-5: a suspend whose export found no native history ended with "the
/// daemon log records the reason under reference ...". The caller now gets a
/// sentence it can act on, carried as a refusal the way J-14's is.
// Hard-won: adfee2e457fb: a Codex session cleared before its first prompt could not suspend and exposed only a log reference
#[test]
fn an_export_with_no_native_history_tells_the_caller_why() {
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        workspace_storage: Default::default(),
    };
    // What the worker printed in R4 (`cli/stop-02-wait.txt`, daemon log).
    let executor = ExportExecutor::new(
        1,
        "2026-09-24T19:48:36Z ERROR mj_worker: Mjolnir worker exited with an error \
         error=\"no session artifacts found\"\nError: no session artifacts found\n",
    );

    let error = run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &export_spec_fixture(),
        export_stdin_command,
        "export target checkpoint",
        None,
    )
    .unwrap_err();

    let refusal = mj_core::refusal::Refusal::of(&error).expect("the reason reaches the caller");
    assert!(
        refusal.message().contains("saved no conversation"),
        "{refusal}"
    );
    // Any other export failure stays internal.
    let executor = ExportExecutor::new(1, "Error: repository '/home/me/app' is missing\n");
    let error = run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &export_spec_fixture(),
        export_stdin_command,
        "export target checkpoint",
        None,
    )
    .unwrap_err();
    assert!(mj_core::refusal::Refusal::of(&error).is_none());
}

/// The explicit export protocol field makes every older worker reject the
/// current spec before it can apply obsolete path or collection behavior.
#[test]
fn a_legacy_export_worker_is_replaced_before_it_runs_obsolete_behavior() {
    for field in ["protocol_version", "consent"] {
        let locator = targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
            workspace_storage: Default::default(),
        };
        let spec = export_spec_fixture();
        let failure = format!(
            "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    unknown field `{field}`, expected `session` at line 1 column 20\n"
        );
        let executor = ExportExecutor::new(1, &failure).retry_stdin_after_failure();
        let directory = tempfile::tempdir().unwrap();
        let worker_binary = directory.path().join("hel-test-worker");
        std::fs::write(&worker_binary, mj_core::worker_build::worker_build_stamp()).unwrap();

        let owner = crate::controller::test_support::worker_test_permit(
            LATCH_RELAY_SESSION,
            "test legacy export",
        )
        .unwrap()
        .unwrap();
        let output = owner
            .scope_blocking(|| {
                run_checkpoint_staging_command(
                    &executor,
                    &locator,
                    LATCH_RELAY_SESSION,
                    &spec,
                    export_stdin_command,
                    "export target checkpoint",
                    Some(&worker_binary),
                )
            })
            .unwrap();

        assert_eq!(output.stdout, exported_checkpoint_json());
        assert_eq!(
            serde_json::from_slice::<CheckpointExportSpec>(&executor.streamed_spec.borrow())
                .unwrap(),
            spec
        );
        // The store's collection runs once per interval for the whole process,
        // so whether this install happened to claim it is not part of the test.
        let purposes: Vec<_> = executor
            .purposes
            .into_inner()
            .into_iter()
            .filter(|purpose| purpose != "collect unused Mjolnir worker binaries")
            .collect();
        assert_eq!(
            purposes,
            vec![
                "export target checkpoint".to_owned(),
                "remove stale private worker staging".to_owned(),
                "link shared Mjolnir worker into session".to_owned(),
                "export target checkpoint".to_owned(),
            ]
        );
    }
}
/// A session that is working is busy, not wedged. A copy that can run
/// again later leaves at once instead of waiting out the deadline, which
/// would restart the worker and kill the turn in flight.
#[test]
fn a_deferral_attached_as_context_under_more_context_is_still_a_deferral() {
    let deferred = anyhow::anyhow!("relay proxy disconnected during hello")
        .context(CheckpointDeferred::background_work())
        .context("connect to the session worker for checkpoint");
    assert!(checkpoint_was_deferred(&deferred), "{deferred:#}");

    let plain = anyhow::anyhow!("relay proxy disconnected during hello")
        .context("connect to the session worker for checkpoint");
    assert!(!checkpoint_was_deferred(&plain), "{plain:#}");
}

#[test]
fn a_working_session_defers_but_close_waits_for_cancellation_before_recovery() {
    let cursor = RelayCursor {
        ordinal: 7,
        digest: "a".repeat(64),
    };
    let mut snapshot = checkpoint_barrier_snapshot(&cursor);
    snapshot.operational.execution = RelayExecutionState::Running;

    let deferred = checkpoint_barrier_wait_ended(
        &snapshot,
        "checkpoint-1",
        BarrierBusyPolicy::DeferWhileRunning,
        HarnessKind::Codex,
        false,
        false,
    )
    .expect("a working session ends the wait at once");
    assert!(checkpoint_was_deferred(&deferred), "{deferred:#}");
    assert!(
        !checkpoint_barrier_needs_worker_restart(&deferred),
        "a deferred copy must never restart the worker: {deferred:#}"
    );
    assert_eq!(
        BarrierBusyPolicy::of(LatchExclusivity::HoldThroughClose),
        BarrierBusyPolicy::InterruptWhileRunning
    );

    // Stop requests a non-steering cancellation and waits for the real
    // turn boundary instead of selecting restart recovery immediately.
    assert!(
        checkpoint_barrier_wait_ended(
            &snapshot,
            "checkpoint-1",
            BarrierBusyPolicy::InterruptWhileRunning,
            HarnessKind::Codex,
            false,
            false,
        )
        .is_none()
    );
    let interrupted = checkpoint_barrier_wait_ended(
        &snapshot,
        "checkpoint-1",
        BarrierBusyPolicy::InterruptWhileRunning,
        HarnessKind::Codex,
        true,
        true,
    )
    .expect("an unresponsive cancellation ends the wait at the deadline");
    assert!(
        checkpoint_barrier_needs_worker_restart(&interrupted),
        "{interrupted:#}"
    );
    assert!(!checkpoint_was_deferred(&interrupted), "{interrupted:#}");

    // A foreground tool can outlive the execution flag: a restart or a stale
    // projection can leave `execution` Idle while a tool is still running. The
    // shared predicate still defers, so a routine copy never restarts the
    // worker underneath the tool.
    snapshot.operational.execution = RelayExecutionState::Idle;
    snapshot.operational.tools_in_flight = vec![mj_core::activity::InFlightToolCall {
        title: None,
        tool_call_id: "bash-1".into(),
        status: agent_client_protocol::v1::schema::ToolCallStatus::InProgress,
        started_at_ms: 1,
    }];
    let tool_deferred = checkpoint_barrier_wait_ended(
        &snapshot,
        "checkpoint-1",
        BarrierBusyPolicy::DeferWhileRunning,
        HarnessKind::Codex,
        true,
        false,
    )
    .expect("a live foreground tool ends the wait at once");
    assert!(checkpoint_was_deferred(&tool_deferred), "{tool_deferred:#}");
    assert!(
        !checkpoint_barrier_needs_worker_restart(&tool_deferred),
        "a live foreground tool must never restart the worker: {tool_deferred:#}"
    );
    snapshot.operational.tools_in_flight.clear();

    // An idle session that never admits the barrier is the real wedge,
    // whatever the policy.
    let wedged = checkpoint_barrier_wait_ended(
        &snapshot,
        "checkpoint-1",
        BarrierBusyPolicy::DeferWhileRunning,
        HarnessKind::Codex,
        true,
        false,
    )
    .expect("the deadline ends the wait");
    assert!(
        checkpoint_barrier_needs_worker_restart(&wedged),
        "{wedged:#}"
    );
    assert!(!checkpoint_was_deferred(&wedged), "{wedged:#}");
}

/// The relay moved on before the controller latched, so the archive would
/// not be an exact cut. That is a deferral, not a failed checkpoint.
#[test]
fn a_frontier_that_moved_before_the_latch_defers_the_checkpoint() {
    let cursor = RelayCursor {
        ordinal: 220,
        digest: "a".repeat(64),
    };
    ensure_exact_checkpoint_cut(&cursor, cursor.ordinal, &cursor.digest)
        .expect("a projection latched at the ready cursor is an exact cut");

    for (ordinal, digest) in [(223, "a".repeat(64)), (220, "b".repeat(64))] {
        let error = ensure_exact_checkpoint_cut(&cursor, ordinal, &digest)
            .expect_err("a projection past the ready cursor is not an exact cut");
        assert!(checkpoint_was_deferred(&error), "{error:#}");
        assert!(
            !checkpoint_barrier_needs_worker_restart(&error),
            "{error:#}"
        );
    }
}

/// The barrier freezes Mjolnir's dispatch, not the harness. A turn the harness
/// started on its own after the cursor was captured may have written to
/// the workspace while it was staged, so that archive is abandoned.
#[test]
fn a_harness_turn_started_during_capture_abandons_the_archive() {
    let cursor = RelayCursor {
        ordinal: 220,
        digest: "a".repeat(64),
    };
    let mut snapshot = checkpoint_barrier_snapshot(&cursor);
    snapshot.operational.checkpoint_ready = Some(cursor.clone());

    snapshot.operational.last_harness_turn_started_ordinal = Some(cursor.ordinal);
    validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor)
        .expect("a turn that started at or before the cursor is covered by the archive");

    snapshot.operational.last_harness_turn_started_ordinal = Some(cursor.ordinal + 1);
    let error = validate_checkpoint_barrier_snapshot(&snapshot, "checkpoint-1", &cursor)
        .expect_err("a turn that started after the cursor invalidates the capture");
    assert!(checkpoint_was_deferred(&error), "{error:#}");
}

/// R4-3: a suspend's checkpoint restarted the worker, the replacement
/// exited because its harness could not start, and the suspend ended as
/// "Error / Recovery unavailable". A checkpoint needs only the relay journal
/// and the files on the target, so a suspend (which holds the latch through
/// close) takes it from a worker started without the harness instead. A
/// routine recovery copy does not: it would leave the session unable to
/// run, and it has nothing to finish.
// Hard-won: 4c5f12f68e8e: a failed harness restart left suspend in Error without a checkpoint fallback
#[test]
fn a_suspend_whose_worker_restart_left_no_worker_checkpoints_without_the_harness() {
    let no_worker = anyhow::anyhow!("write relay history_requests request: Broken pipe")
        .context(crate::controller::worker_restart::WorkerRestartLeftNoWorker)
        .context("wait for ACP session after restarting the worker for checkpoint");
    assert!(restart_falls_back_to_checkpoint_only(
        LatchExclusivity::HoldThroughClose,
        &no_worker
    ));
    assert!(!restart_falls_back_to_checkpoint_only(
        LatchExclusivity::ReleaseAfterLatch,
        &no_worker
    ));
    let stop_failed = anyhow::anyhow!("stop wedged Mjolnir worker before retrying checkpoint");
    assert!(
        !restart_falls_back_to_checkpoint_only(LatchExclusivity::HoldThroughClose, &stop_failed),
        "a worker that may still run is not replaced by a second one"
    );
}

#[test]
fn a_stuck_checkpoint_barrier_is_retried_by_restarting_the_worker() {
    // Both ways the wait can end without a barrier, each wrapped the way
    // the checkpoint path wraps them, and each still asking for the retry.
    let not_admitted =
        CheckpointBarrierUnreachable::not_admitted("checkpoint-976f6746887c5ccd93b9d8bbe120ef06");
    assert_eq!(
        not_admitted.to_string(),
        "ACP relay did not reach checkpoint barrier checkpoint-976f6746887c5ccd93b9d8bbe120ef06"
    );
    for failure in [
        not_admitted,
        CheckpointBarrierUnreachable::runtime_stopped(),
    ] {
        let error = anyhow::Error::new(failure).context("latch a session checkpoint");
        assert!(checkpoint_barrier_needs_worker_restart(&error), "{error:#}");
    }
    assert!(!checkpoint_barrier_needs_worker_restart(&anyhow::anyhow!(
        "export target checkpoint failed with status 1"
    )));
    // The decision reads the type, not the text, so the old wording alone
    // no longer restarts a worker and rewording one cannot stop it either.
    assert!(!checkpoint_barrier_needs_worker_restart(&anyhow::anyhow!(
        "ACP relay did not reach checkpoint barrier checkpoint-1"
    )));
}

#[test]
fn an_incompatible_cancel_turn_requests_worker_recovery() {
    let error = anyhow::Error::new(RelayRejected(mj_core::relay::RelayProtocolError {
        code: mj_core::relay::RelayErrorCode::IncompatibleProtocol,
        message: "request uses protocol 6".into(),
        retryable: false,
        detail: None,
    }))
    .context("cancel active ACP turn before checkpoint barrier");
    assert!(
        checkpoint_cancel_turn_needs_worker_restart(&error),
        "{error:#}"
    );
    assert!(checkpoint_barrier_needs_worker_restart(&error.context(
        CheckpointBarrierUnreachable::cancel_turn_unavailable("checkpoint-1", 6,)
    )));
}
// Hard-won: 0fc100bb4566: a dead worker hello prevented a later Stop from reaching the latch retry
#[test]
fn a_dead_worker_hello_failure_is_retried_by_restarting_the_worker() {
    let dead = anyhow::Error::new(RelayTransportDead::new("the proxy is gone"))
        .context("connect to the session worker for checkpoint");
    assert!(worker_connect_needs_restart(&dead), "{dead:#}");
    assert!(!worker_connect_needs_restart(&anyhow::anyhow!(
        "unknown session"
    )));
}

#[test]
fn export_spec_schema_mismatch_is_detected_from_the_parse_error() {
    assert!(export_spec_schema_unsupported(
        "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    \
             unknown field `terminal_refs`, expected `call` at line 1 column 7276552\n"
    ));
    assert!(export_spec_schema_unsupported(
        "Error: parse checkpoint export spec /spec.json\n\nCaused by:\n    \
             unknown variant `terminal_output`, expected one of `user`, `agent`\n"
    ));
    assert!(!export_spec_schema_unsupported(
        "Error: repository 'app' is missing\n"
    ));
    assert!(!export_spec_schema_unsupported(
        "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    \
             missing field `relay_root`\n"
    ));
    assert!(export_protocol_unsupported(
        "Error: unsupported checkpoint export protocol version 3; worker supports 2\n"
    ));
}
#[cfg(unix)]
const LATCH_TEST_CHILD: &str = "MJ_TEST_LATCH_CHILD";
#[cfg(unix)]
#[cfg(unix)]
const RELEASE_TEST_CHILD: &str = "MJ_TEST_RELEASE_LATCH_CHILD";
#[cfg(unix)]
const LEGACY_RELEASE_TEST_CHILD: &str = "MJ_TEST_LEGACY_RELEASE_LATCH_CHILD";
#[cfg(unix)]
const REUSE_TEST_CHILD: &str = "MJ_TEST_REUSE_LATCH_CHILD";
#[cfg(unix)]
const STAGE_CLEANUP_TEST_CHILD: &str = "MJ_TEST_STAGE_CLEANUP_CHILD";
#[cfg(unix)]
const CLOSE_CUT_TEST_CHILD: &str = "MJ_TEST_CLOSE_CUT_CHILD";

/// Start a session manager against a live relay and latch a checkpoint on
/// it, exactly as [`Controller::checkpoint_session_latched`] does.
#[cfg(unix)]
async fn latch_a_live_checkpoint(
    relay_root: &Path,
    starts: Option<&Path>,
    release: ReleaseSupport,
    running: bool,
) -> (
    crate::session_manager::SessionManagerChannels,
    ManagedSessionHandle,
    ControllerRelayLease,
    String,
    RelayCursor,
) {
    // The projection refuses events for sessions the controller does not
    // know, so register the one the relay journals for.
    crate::database::save_session(&checkpoint_test_session(LATCH_RELAY_SESSION)).unwrap();
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            relay_root, starts, release, running,
        )])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();

    let lease = handle
        .lease_connection(&crate::controller::test_support::session_admission(
            handle.session_id(),
        ))
        .await
        .unwrap();
    let mut relay = ControllerRelayLease::Managed {
        handle: handle.clone(),
        lease: Some(lease),
    };
    let barrier_command_id = new_command_id("checkpoint").unwrap();
    let connection = relay.connection_mut();
    connection
        .submit(
            barrier_command_id.clone(),
            RelayCommand::BeginCheckpoint { reason: None },
        )
        .await
        .unwrap();
    let barrier = wait_for_checkpoint_barrier(
        connection,
        LATCH_RELAY_SESSION,
        &barrier_command_id,
        CHECKPOINT_BARRIER_TIMEOUT,
        BarrierBusyPolicy::InterruptWhileRunning,
        HarnessKind::Codex,
    )
    .await
    .unwrap();
    assert_eq!(
        barrier.materialized.applied_event_ordinal,
        barrier.operational.latest_ordinal
    );
    let cursor = barrier.operational.checkpoint_ready.clone().unwrap();
    (channels, handle, relay, barrier_command_id, cursor)
}

#[cfg(unix)]
#[tokio::test]
async fn worker_upgrade_defers_a_busy_turn_and_keeps_steering_on_the_same_connection() {
    if std::env::var_os(LATCH_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::worker_upgrade_defers_a_busy_turn_and_keeps_steering_on_the_same_connection",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(LATCH_TEST_CHILD, "1")
            .env("MJ_INSTANCE", "upgrade-in-flight-test")
            .env(LATCH_RELAY_MAX_PROTOCOL, "58")
            .env(LATCH_REJECT_PROJECT_MEMORY, "1")
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let starts = root.path().join("starts");
    crate::database::save_session(&checkpoint_test_session(LATCH_RELAY_SESSION)).unwrap();
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    let mut target =
        latch_relay_target(root.path(), Some(&starts), ReleaseSupport::Supported, true);
    target.project_memory = Some(crate::session_manager::ProjectMemorySyncTarget {
        canonical_root: root.path().join("memory"),
    });
    channels.targets.send(vec![target]).unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        IdleWorkspaceLease::acquire_for_upgrade(
            &crate::controller::test_support::session_admission(handle.session_id()),
            &handle,
            HarnessKind::Codex
        )
        .await
        .unwrap()
        .is_none()
    );
    handle
        .submit(
            "steering-input".into(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("change direction")],
            },
        )
        .await
        .unwrap();
    handle
        .submit(
            "steer-running".into(),
            RelayCommand::Steer {
                active_prompt_id: "seed-running-prompt".into(),
                queued_prompt_id: "steering-input".into(),
            },
        )
        .await
        .unwrap();
    handle.sync_now().await.unwrap();
    let snapshot = handle.view().snapshot.unwrap();
    assert_eq!(snapshot.operational.relay_protocol_version, Some(58));
    assert!(snapshot.operational.checkpoint_barrier.is_none());
    assert_eq!(
        snapshot.operational.steering.as_ref().unwrap().command_id,
        "steer-running"
    );
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap().lines().count(),
        1,
        "busy deferral must return without detaching or restarting the connection"
    );
    channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: 6984aae: live Resume waited for readiness before discarding queued work.
#[cfg(unix)]
#[tokio::test]
async fn live_resume_discards_queue_before_native_readiness() {
    const CHILD: &str = "MJ_RESUME_DISCARD_READY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            "live_resume_discards_queue_before_native_readiness",
        ))
        .env(CHILD, "1")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("resume-discard-before-ready"), b"").unwrap();
    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.clone(),
    }));
    crate::database::save_session(&session).unwrap();
    let controller = Controller {
        config: Default::default(),
        state: mj_core::state::State {
            environment_owners: [(LATCH_RELAY_SESSION.into(), LATCH_RELAY_SESSION.into())]
                .into_iter()
                .collect(),
            sessions: [(LATCH_RELAY_SESSION.into(), session)]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    };
    let target = latch_relay_target(&root, None, ReleaseSupport::Supported, false);
    let backend = targets::TargetLocator::LocalBare {
        worker_root: root.to_string_lossy().into_owned(),
    };
    let owner = crate::controller::test_support::worker_test_permit(
        LATCH_RELAY_SESSION,
        "test discard readiness",
    )
    .unwrap()
    .unwrap();
    let result = owner
        .scope(tokio::time::timeout(
            Duration::from_secs(10),
            controller.observe_started_worker(
                LATCH_RELAY_SESSION,
                &targets::ProcessExecutor,
                super::super::worker_restart::InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &root.to_string_lossy(),
                    reconnect: &target.spec,
                    launch: None,
                    prepared: true,
                    discard_queued_prompts: Some("discard-before-ready".into()),
                    staged_aws_profile: None,
                    swap_admission: None,
                    messages: &super::super::worker_restart::RESTART_FOR_CHECKPOINT,
                },
            ),
        ))
        .await;
    let connection = result
        .expect("queue discard must unblock native readiness")
        .unwrap();
    drop(connection);
}

#[cfg(unix)]
#[tokio::test]
async fn a_ready_unchanged_worker_settles_an_abandoned_boot_on_the_next_sync() {
    const CHILD: &str = "MJ_QUIET_RESTART_READY_CHILD";
    let name = "a_ready_unchanged_worker_settles_an_abandoned_boot_on_the_next_sync";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(crate::controller::test_support::test_name(
            module_path!(),
            name,
        ))
        .env(CHILD, "1")
        .env(LATCH_CHECKPOINT_ONLY, "1")
        .env("MJ_INSTANCE", "worker-owner-quiet-readiness")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(LATCH_RELAY_SESSION);
    std::fs::create_dir_all(&root).unwrap();
    drop(mj_worker::relay::DurableRelay::open(&root, LATCH_RELAY_SESSION, "1.0.0").unwrap());
    let durable_target = TargetLocator::LocalBare {
        worker_root: root.clone(),
    };
    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target = Some(Box::new(durable_target.clone()));
    crate::database::save_session(&session).unwrap();
    let mut target = latch_relay_target(&root, None, ReleaseSupport::Supported, false);
    let restarted = root.join("unexpected-restart");
    target.worker_recovery = Some(crate::session_manager::WorkerRecoveryPlan {
        source_target: durable_target.clone(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", ["alive\n"]).into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: targets::CommandPlan {
            description: "unexpected restart".into(),
            commands: vec![CommandSpec::new(
                "touch",
                [restarted.to_string_lossy().into_owned()],
            )],
        }
        .into(),
    });
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels.targets.send(vec![target]).unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    handle.sync_now().await.unwrap();
    let ordinal = handle.view().snapshot.unwrap().operational.latest_ordinal;
    let admission = channels
        .control
        .session_gate
        .try_start(
            LATCH_RELAY_SESSION,
            crate::recovery_gate::BackgroundWork::WorkerReplacement,
        )
        .unwrap();
    let owner =
        crate::worker_lifecycle::WorkerPermit::acquire(LATCH_RELAY_SESSION, &admission).unwrap();
    drop(admission);
    owner
        .scope_blocking(|| {
            owner.begin_restart(&durable_target, String::new())?;
            crate::database::advance_worker_restart(
                LATCH_RELAY_SESSION,
                owner.operation_id(),
                crate::database::WorkerRestartPhase::Swapping,
            )?;
            crate::database::advance_worker_restart(
                LATCH_RELAY_SESSION,
                owner.operation_id(),
                crate::database::WorkerRestartPhase::AwaitingReadiness,
            )
        })
        .unwrap();
    handle.sync_now().await.unwrap();
    assert!(
        crate::database::load_worker_restart(LATCH_RELAY_SESSION)
            .unwrap()
            .is_some()
    );
    drop(owner);
    handle.sync_now().await.unwrap();
    assert_eq!(
        handle.view().snapshot.unwrap().operational.latest_ordinal,
        ordinal
    );
    assert!(
        crate::database::load_worker_restart(LATCH_RELAY_SESSION)
            .unwrap()
            .is_none()
    );
    assert!(!restarted.exists());
    channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1611: Suspend could not reserve a legacy Antigravity worker to upgrade its suspend capability.
#[cfg(unix)]
#[tokio::test]
async fn explicit_suspend_can_reserve_an_idle_legacy_antigravity_worker() {
    if std::env::var_os(LATCH_TEST_CHILD).is_none() {
        let test_name = format!(
            "{}::explicit_suspend_can_reserve_an_idle_legacy_antigravity_worker",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name)
            .env(LATCH_TEST_CHILD, "1")
            .env("MJ_INSTANCE", "legacy-antigravity-suspend-test")
            .env(LATCH_RELAY_MAX_PROTOCOL, "83")
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.harness_kind = HarnessKind::Antigravity;
    crate::database::save_session(&session).unwrap();
    let mut relay =
        mj_worker::relay::DurableRelay::open(root.path(), LATCH_RELAY_SESSION, "1.0.0").unwrap();
    relay
        .record_observation(mj_core::relay::RelayObservation::SessionOpened {
            native_session_id: "legacy-antigravity".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    relay
        .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    // The stand-in's internal idle reservation uses the Codex harness policy.
    relay
        .record_session_update(
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "session_info_update",
                "_meta": {"execution": {"version": 1, "status": "idle"}}
            }))
            .unwrap(),
        )
        .unwrap();
    drop(relay);
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            root.path(),
            None,
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    wait_until_the_actor_serves_again(&handle).await;
    let admission = crate::controller::test_support::session_admission(handle.session_id());
    assert!(
        IdleWorkspaceLease::acquire_for_upgrade(&admission, &handle, HarnessKind::Antigravity)
            .await
            .unwrap()
            .is_none()
    );
    let mut lease = IdleWorkspaceLease::acquire_for_replacement(
        &admission,
        &handle,
        HarnessKind::Antigravity,
        mj_core::activity::WorkerReplacement::Other,
    )
    .await
    .unwrap()
    .expect("explicit Suspend can reserve the quiet old worker");
    assert!(lease.verify_for_upgrade().await.unwrap());
    lease.release().await.unwrap();
    wait_until_the_actor_serves_again(&handle).await;
    let mut write = IdleWorkspaceLease::acquire(&admission, &handle, HarnessKind::Antigravity)
        .await
        .expect("an idle legacy worker can still admit a workspace write");
    write.verify().await.unwrap();
    write.release().await.unwrap();
    channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1498: idle goal upgrades must preserve reviewer admission and reject later native turns.
#[cfg(unix)]
#[tokio::test]
async fn worker_upgrade_reservation_releases_after_abandonment() {
    if std::env::var_os(LATCH_TEST_CHILD).is_none() {
        let test_name = format!(
            "{}::worker_upgrade_reservation_releases_after_abandonment",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        for protocol in [58, 59, 60] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name.clone())
                .env(LATCH_TEST_CHILD, protocol.to_string())
                .env("MJ_INSTANCE", "upgrade-in-flight-test")
                .env(LATCH_RELAY_MAX_PROTOCOL, protocol.to_string())
                .env(LATCH_REJECT_PROJECT_MEMORY, "1")
                .isolated_store(directory.path())
                .run();
        }
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let protocol: u32 = std::env::var(LATCH_TEST_CHILD).unwrap().parse().unwrap();
    std::fs::write(
        root.path().join("upgrade-goal-peer"),
        b"scoped legacy upgrade peer",
    )
    .unwrap();
    crate::database::save_session(&checkpoint_test_session(LATCH_RELAY_SESSION)).unwrap();
    let mut relay =
        mj_worker::relay::DurableRelay::open(root.path(), LATCH_RELAY_SESSION, "1.0.0").unwrap();
    relay
        .record_observation(mj_core::relay::RelayObservation::SessionOpened {
            native_session_id: "native-session".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    relay
        .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    relay.record_session_update(serde_json::from_value(serde_json::json!({
        "sessionUpdate":"session_info_update", "_meta": {
            "goal":{"objective":"Retain the idle durable goal", "status":"active", "createdAt":42},
            "execution":{"version":1,"status":"idle"}
        }
    })).unwrap()).unwrap();
    let preserved_goal = relay.operational_state().goal;
    drop(relay);
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    let mut target = latch_relay_target(root.path(), None, ReleaseSupport::Supported, false);
    target.project_memory = Some(crate::session_manager::ProjectMemorySyncTarget {
        canonical_root: root.path().join("memory"),
    });
    channels.targets.send(vec![target]).unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    wait_until_the_actor_is_safe_to_replace(&handle, HarnessKind::Codex).await;
    // The peer's private reviewer admission is deliberately absent from its
    // public activity snapshot. ReserveIdle must still confirm its absence.
    let reviewer = root.path().join("upgrade-held-reviewer");
    std::fs::write(&reviewer, b"review capture still running").unwrap();
    assert!(
        IdleWorkspaceLease::acquire_for_upgrade(
            &crate::controller::test_support::session_admission(handle.session_id()),
            &handle,
            HarnessKind::Codex
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        reviewer.exists(),
        "deferral must leave admitted review work alive"
    );
    handle.sync_now().await.unwrap();
    assert_eq!(
        handle.view().snapshot.unwrap().operational.goal,
        preserved_goal
    );
    std::fs::remove_file(reviewer).unwrap();
    wait_until_the_actor_is_safe_to_replace(&handle, HarnessKind::Codex).await;
    let mut lease = IdleWorkspaceLease::acquire_for_upgrade(
        &crate::controller::test_support::session_admission(handle.session_id()),
        &handle,
        HarnessKind::Codex,
    )
    .await
    .unwrap()
    .expect("idle worker");
    assert_eq!(lease.protocol_version(), protocol);
    assert!(lease.verify_for_upgrade().await.unwrap());
    assert_eq!(
        lease.lease.connection_mut().operational().goal,
        preserved_goal
    );
    // Native AIR can arrive while the barrier is held. A start and finish
    // after ready invalidates its original cut even though the worker is idle again.
    std::fs::write(root.path().join("upgrade-late-turn"), b"start and finish").unwrap();
    assert!(!lease.verify_for_upgrade().await.unwrap());
    let operational = lease.lease.connection_mut().operational();
    assert!(operational.checkpoint_barrier.is_some());
    assert_eq!(operational.execution, RelayExecutionState::Idle);
    assert!(operational.harness_turn.is_none());
    assert_eq!(operational.goal, preserved_goal);
    drop(lease);
    wait_until_the_actor_is_safe_to_replace(&handle, HarnessKind::Codex).await;
    let mut lease = IdleWorkspaceLease::acquire_for_upgrade(
        &crate::controller::test_support::session_admission(handle.session_id()),
        &handle,
        HarnessKind::Codex,
    )
    .await
    .unwrap()
    .expect("abandoned barrier recovered");
    assert!(lease.verify_for_upgrade().await.unwrap());
    assert_eq!(
        lease.lease.connection_mut().operational().goal,
        preserved_goal
    );
    let first = handle
        .enqueue_submit(
            "first-after-upgrade".into(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("first queued turn")],
            },
        )
        .await
        .unwrap();
    let second = handle
        .enqueue_submit(
            "second-after-upgrade".into(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("second queued turn")],
            },
        )
        .await
        .unwrap();
    lease.release().await.unwrap();
    let first_ordinal = first.wait().await.unwrap();
    let second_ordinal = second.wait().await.unwrap();
    assert!(
        first_ordinal < second_ordinal,
        "queued prompts retain their identity and order"
    );
    handle.sync_now().await.unwrap();
    assert_eq!(
        handle.view().snapshot.unwrap().operational.goal,
        preserved_goal
    );
    let requests = std::fs::read_to_string(root.path().join("upgrade-requests.log")).unwrap();
    assert!(
        requests.lines().any(|name| name == "reserve_idle"),
        "all versions confirm reviewer admission through ReserveIdle"
    );
    assert_eq!(
        requests.lines().any(|name| name == "begin_checkpoint"),
        protocol < mj_core::relay::RELAY_IDLE_GOAL_INTENT_PROTOCOL
    );
    assert!(!requests.lines().any(|name| name == "cancel_turn"));
    if protocol < mj_core::relay::RELAY_IDLE_GOAL_INTENT_PROTOCOL {
        let begin = requests
            .lines()
            .position(|name| name == "begin_checkpoint")
            .unwrap();
        let confirm = requests
            .lines()
            .position(|name| name == "reserve_idle")
            .unwrap();
        assert!(
            begin < confirm,
            "legacy admission establishes the barrier before reviewer confirmation"
        );
    }
    channels.shutdown.shutdown().await.unwrap();
}

/// A close checkpoint cancels an active prompt and waits for the prompt's
/// terminal event before admitting its barrier. The scripted worker clears
/// the prompt only when it receives `CancelTurn`, so a single-start log
/// proves that a responsive cancellation did not take restart recovery.
#[cfg(unix)]
#[tokio::test]
async fn a_close_checkpoint_cancels_a_running_turn_without_restarting_the_worker() {
    // MJ_DATA_DIR is process-global, so keep the database-backed relay
    // actor isolated from unrelated tests.
    if std::env::var_os(LATCH_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_close_checkpoint_cancels_a_running_turn_without_restarting_the_worker",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(LATCH_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let relay_root = tempfile::tempdir().unwrap();
    let start_log_directory = tempfile::tempdir().unwrap();
    let start_log = start_log_directory.path().join("relay-starts");
    let (_channels, _handle, mut relay, _barrier_command_id, _cursor) = latch_a_live_checkpoint(
        relay_root.path(),
        Some(&start_log),
        ReleaseSupport::Supported,
        true,
    )
    .await;
    let snapshot = relay.sync_snapshot().await.unwrap();
    assert_eq!(
        snapshot.operational.execution,
        RelayExecutionState::Idle,
        "the close wait returned before the cancelled turn became idle"
    );
    assert!(
        snapshot.operational.active_prompt.is_none(),
        "the close wait returned before the cancelled prompt settled"
    );
    assert_eq!(
        relay_starts(&start_log),
        1,
        "responsive cancellation restarted worker"
    );
}
/// `wait_for_session` publishes the actor before its first relay sync finishes.
#[cfg(unix)]
async fn wait_until_the_actor_is_safe_to_replace(
    handle: &ManagedSessionHandle,
    harness: HarnessKind,
) {
    for attempt in 0..1000 {
        let served = handle.sync_now().await.is_ok();
        let snapshot = handle.view().snapshot;
        if served
            && snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.safe_to_replace(harness))
        {
            return;
        }
        assert!(
            attempt < 999,
            "worker never became safe to replace: {:?}",
            snapshot.map(|snapshot| snapshot.operational.facts())
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    unreachable!("the readiness loop returns or panics")
}
/// Ends a held checkpoint barrier the way a close's CompleteCheckpoint
/// decision does, which is what hands ACP dispatch back and moves the floor.
#[cfg(unix)]
async fn complete_held_barrier(relay: &mut ControllerRelayLease, barrier_command_id: &str) {
    relay
        .submit(
            new_command_id("checkpoint-complete").unwrap(),
            RelayCommand::CompleteCheckpoint {
                barrier_command_id: barrier_command_id.to_owned(),
            },
        )
        .await
        .unwrap();
}
/// Ending the latch is the whole point of the split checkpoint: the actor
/// serves the dashboard again while the archive is still being exported,
/// and the events it accepts do not invalidate the latched archive.
#[cfg(unix)]
#[tokio::test]
async fn ending_the_checkpoint_latch_returns_the_connection_to_its_actor() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(LATCH_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::ending_the_checkpoint_latch_returns_the_connection_to_its_actor",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(LATCH_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    // A connection that never comes back would hang the suite instead of
    // failing it, so turn a stall into a hard error.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the checkpoint latch never returned its connection");
        std::process::exit(101);
    });

    let relay_root = tempfile::tempdir().unwrap();
    let (_channels, handle, mut relay, barrier_command_id, cursor) =
        latch_a_live_checkpoint(relay_root.path(), None, ReleaseSupport::Supported, false).await;

    // Latch phase: the projection must be read at the exact ready cursor,
    // so the actor cannot reach the relay at all.
    assert!(
        handle.sync_now().await.is_err(),
        "a latched projection must not be advanced by its own actor"
    );

    relay.end_latch();
    wait_until_the_actor_serves_again(&handle).await;

    // Slow phase, before anything else reaches the relay: the controller
    // reads its barrier back through the actor, which must report what the
    // latch already applied.
    let latched = relay.sync_snapshot().await.unwrap();
    validate_checkpoint_barrier_snapshot(&latched, &barrier_command_id, &cursor).unwrap();

    // A prompt accepted while the archive transfers moves the frontier past
    // the ready cursor. The barrier still seals the same workspace.
    let prompt_ordinal = relay
        .submit(
            new_command_id("prompt").unwrap(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
        )
        .await
        .unwrap();
    assert!(prompt_ordinal > cursor.ordinal);
    let snapshot = relay.sync_snapshot().await.unwrap();
    assert!(snapshot.operational.latest_ordinal > cursor.ordinal);
    validate_checkpoint_barrier_snapshot(&snapshot, &barrier_command_id, &cursor).unwrap();

    complete_held_barrier(&mut relay, &barrier_command_id).await;
    handle.sync_now().await.unwrap();
    assert_eq!(
        handle
            .view()
            .snapshot
            .expect("the actor published the completed barrier")
            .operational
            .checkpoint_barrier,
        None
    );
}
/// The archive is complete once the export returns, so the harness stops
/// waiting there: the barrier ends and ACP dispatch resumes. ACK-only workers
/// need no delayed archive-coverage update after installation.
#[cfg(unix)]
#[tokio::test]
async fn releasing_a_checkpoint_after_capture_needs_no_floor_move_on_ack_only_workers() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(RELEASE_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::releasing_a_checkpoint_after_capture_needs_no_floor_move_on_ack_only_workers",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(RELEASE_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    // A barrier that never releases would hang the suite instead of failing
    // it, so turn a stall into a hard error.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the captured checkpoint never released its barrier");
        std::process::exit(101);
    });

    let relay_root = tempfile::tempdir().unwrap();
    let (_channels, handle, mut relay, barrier_command_id, cursor) =
        latch_a_live_checkpoint(relay_root.path(), None, ReleaseSupport::Supported, false).await;
    relay.end_latch();
    wait_until_the_actor_serves_again(&handle).await;

    // Target state capture has just finished. Releasing proves the barrier first and
    // then hands ACP dispatch back.
    let completion = release_checkpoint_after_capture(
        &mut relay,
        LATCH_RELAY_SESSION,
        &barrier_command_id,
        &cursor,
        HarnessKind::Codex,
    )
    .await
    .unwrap();
    assert_eq!(completion, CheckpointCompletion::ReleasedAfterCapture);
    let released = relay.sync_snapshot().await.unwrap();
    assert_eq!(released.operational.checkpoint_barrier, None);
    assert_eq!(released.operational.checkpoint_ready, None);
    assert_eq!(
        released.operational.recovery_floor_ordinal, 0,
        "capture release leaves the legacy archive-coverage marker alone"
    );

    // The transfer is still running, and the harness is already working
    // again: a prompt submitted now reaches ACP dispatch.
    relay
        .submit(
            new_command_id("prompt").unwrap(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("during transfer"))],
            },
        )
        .await
        .unwrap();
    let mut dispatched = None;
    for attempt in 0.. {
        let snapshot = relay.sync_snapshot().await.unwrap();
        if let Some(active) = &snapshot.operational.active_prompt {
            dispatched = Some(active.clone());
            break;
        }
        assert!(attempt < 200, "a released barrier still froze ACP dispatch");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(dispatched.is_some());

    // Installation has no additional journal-retention work on current workers.
    handle.sync_now().await.unwrap();
    let installed = handle
        .view()
        .snapshot
        .expect("the actor remains attached after archive installation");
    assert_eq!(installed.operational.recovery_floor_ordinal, 0);
    assert_eq!(
        installed.operational.recovery_floor_digest,
        mj_core::relay::RELAY_EVENT_GENESIS_DIGEST
    );
}
/// A target still running a worker that predates the early release keeps
/// its barrier through the transfer and ends it the way it always did.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_that_rejects_the_release_keeps_its_barrier_through_the_transfer() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(LEGACY_RELEASE_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_worker_that_rejects_the_release_keeps_its_barrier_through_the_transfer",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(LEGACY_RELEASE_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    // A rejected release that lost its barrier would hang the suite instead
    // of failing it, so turn a stall into a hard error.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the rejected release never finished its checkpoint");
        std::process::exit(101);
    });

    let relay_root = tempfile::tempdir().unwrap();
    let start_log = tempfile::tempdir().unwrap();
    let start_log = start_log.path().join("relay-starts");
    let (_channels, handle, mut relay, barrier_command_id, cursor) = latch_a_live_checkpoint(
        relay_root.path(),
        Some(&start_log),
        ReleaseSupport::Rejected,
        false,
    )
    .await;
    relay.end_latch();
    wait_until_the_actor_serves_again(&handle).await;

    let completion = release_checkpoint_after_capture(
        &mut relay,
        LATCH_RELAY_SESSION,
        &barrier_command_id,
        &cursor,
        HarnessKind::Codex,
    )
    .await
    .unwrap();
    assert_eq!(completion, CheckpointCompletion::HeldBarrier);
    // A refused command is a completed round trip, so the connection that
    // owns the barrier must survive it.
    assert_eq!(relay_starts(&start_log), 1);

    // Today's ordering carries on: the barrier holds through the transfer,
    // the post-transfer revalidation still has something to prove, and the
    // completion both resumes dispatch and advances the recovery floor.
    let transferring = relay.sync_snapshot().await.unwrap();
    validate_checkpoint_barrier_snapshot(&transferring, &barrier_command_id, &cursor).unwrap();
    complete_held_barrier(&mut relay, &barrier_command_id).await;
    handle.sync_now().await.unwrap();
    let completed = handle
        .view()
        .snapshot
        .expect("the actor published the completed barrier");
    assert_eq!(completed.operational.checkpoint_barrier, None);
    assert_eq!(completed.operational.recovery_floor_ordinal, cursor.ordinal);
}
/// The close policy, end to end against a live relay: a latch that finds
/// its own content already archived issues no export or transfer command
/// and keeps the installed archive, while the next latch after real
/// session content goes back through the full export.
#[cfg(unix)]
#[test]
fn a_move_checkpoint_can_verify_its_archive_without_source_harness_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let name = format!(
        "{}::a_close_latch_reuses_an_unchanged_archive_and_exports_after_new_content",
        module_path!()
            .strip_prefix("mj_controller::")
            .unwrap_or(module_path!())
    );
    IsolatedTest::new(name)
        .env(REUSE_TEST_CHILD, "1")
        .env(LATCH_CHECKPOINT_ONLY, "1")
        .env("MJ_DATA_DIR", directory.path())
        .run();
}

#[cfg(unix)]
const QUEUED_WAKE_TEST_CHILD: &str = "MJ_TEST_QUEUED_WAKE_CHILD";

/// A worker that restarts during a mailbox wake requeues the wake as its
/// restart continuation, and saving the failed session checkpoints that queue
/// without a harness. The daemon's projection keeps no payload for a wake, so
/// the latch archives the prompt the worker's queue holds at the cut. A
/// checkpoint-only worker too old to report pending context cannot safely save
/// a checkpoint, even if it can report the wake.
// Hard-won: #1594: a queued mailbox wake failed every checkpoint export with "has no content".
// Hard-won: pending handoff text was absent from suspend checkpoints.
#[cfg(unix)]
#[tokio::test]
async fn a_checkpoint_archives_a_queued_mailbox_wake_from_the_worker_queue() {
    let reads_queue = mj_core::relay::RELAY_CHECKPOINT_MAILBOX_PROTOCOL;
    let Some(protocol) = std::env::var_os(QUEUED_WAKE_TEST_CHILD) else {
        for protocol in [
            mj_core::relay::RELAY_PROTOCOL_VERSION,
            mj_core::relay::RELAY_CHECKPOINT_PROMPT_CONTEXT_PROTOCOL - 1,
            reads_queue - 1,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::a_checkpoint_archives_a_queued_mailbox_wake_from_the_worker_queue",
                module_path!()
                    .strip_prefix("mj_controller::")
                    .unwrap_or(module_path!())
            );
            IsolatedTest::new(test_name)
                .env(QUEUED_WAKE_TEST_CHILD, protocol.to_string())
                .env(LATCH_RELAY_MAX_PROTOCOL, protocol.to_string())
                .env(LATCH_CHECKPOINT_ONLY, "1")
                .env("MJ_DATA_DIR", directory.path())
                .run();
        }
        return;
    };
    let protocol: u32 = protocol.to_str().unwrap().parse().unwrap();
    let pending_context = "hidden checkpoint context ".repeat(4_000);
    let _writer = crate::database::install_isolated_test_writer();
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the queued-wake checkpoint never returned");
        std::process::exit(101);
    });

    /// Refuses every target command, keeping the export spec it was given
    /// and the notices it showed.
    #[derive(Default)]
    struct ExportRecorder {
        exported: std::sync::Mutex<Option<CheckpointExportSpec>>,
        notices: std::sync::Mutex<Vec<String>>,
    }

    impl CommandExecutor for ExportRecorder {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"no target is provisioned for this test".to_vec(),
            })
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            if command.purpose == "export target checkpoint" {
                *self.exported.lock().unwrap() = Some(serde_json::from_reader(input)?);
            }
            self.execute(command)
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let relay_root = data_directory.join("relay");
    let profile_home = data_directory.join("profile");
    for directory in [&relay_root, &profile_home] {
        std::fs::create_dir_all(directory).unwrap();
    }
    // A parent's waking message started a turn, and the worker restarted
    // before it finished.
    let wake_prompt = {
        let mut relay =
            mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0")
                .unwrap();
        relay
            .record_observation(mj_core::relay::RelayObservation::SessionOpened {
                native_session_id: "native-session".into(),
                native_continuity_lost: false,
                replaced_unused_native_session_id: None,
                resumed: true,
            })
            .unwrap();
        relay
            .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        let installed = relay.handle(mj_core::relay::RelayRequestEnvelope {
            request_id: "install-context".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: mj_core::relay::RelayRequest::InstallPromptContext {
                text: pending_context.clone(),
            },
        });
        assert!(
            matches!(installed.body, mj_core::relay::RelayResponseBody::Ok { .. }),
            "install pending prompt context: {installed:?}"
        );
        let delivered = relay.handle(mj_core::relay::RelayRequestEnvelope {
            request_id: "deliver-parent-task".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: mj_core::relay::RelayRequest::Submit {
                command_id: "deliver-parent-task".into(),
                command: RelayCommand::DeliverMailboxEvent {
                    event: mj_core::mailbox::MailboxEvent {
                        key: "parent-task".into(),
                        source: "parent".into(),
                        wake: true,
                        created_at_ms: 1,
                        body: mj_core::mailbox::MailboxEventBody::PlainText {
                            text: "Review the migration and report what breaks.".into(),
                        },
                    },
                },
            },
        });
        assert!(
            matches!(delivered.body, mj_core::relay::RelayResponseBody::Ok { .. }),
            "{delivered:?}"
        );
        let started = relay.claim_pending_commands(true).unwrap();
        let [wake] = started.as_slice() else {
            panic!("expected only the mailbox wake to start: {started:?}");
        };
        assert!(matches!(wake.command, RelayCommand::MailboxWake { .. }));
        wake.command.prompt_blocks().unwrap().into_owned()
    };
    drop(mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0").unwrap());

    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target_template_id = "removed-local".into();
    session.target_runtime = Some((&TargetTemplate::LocalBare).into());
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: data_directory.join("workers").join(LATCH_RELAY_SESSION),
    }));
    crate::database::save_session(&session).unwrap();
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile::new(mj_core::config::HarnessKind::Codex, profile_home),
    );
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: Some("example/project".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
                mbx_namespace: None,
            }],
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [(LATCH_RELAY_SESSION.into(), LATCH_RELAY_SESSION.into())]
                .into_iter()
                .collect(),
            sessions: [(LATCH_RELAY_SESSION.into(), session)]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            &relay_root,
            None,
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();

    let executor = ExportRecorder::default();
    let result = controller
        .checkpoint_session_latched_for_operation_with_delivery(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::HoldThroughClose,
            CheckpointExportPolicy::ReuseUnchangedArchive,
            CheckpointDelivery::DownloadToDaemon,
            None,
            None,
            &mj_core::suspend_consent::CaptureConsent::default(),
        )
        .await;
    let Err(error) = result else {
        panic!("a checkpoint exported to a target that refuses every command");
    };
    if protocol < mj_core::relay::RELAY_CHECKPOINT_PROMPT_CONTEXT_PROTOCOL {
        assert!(
            format!("{error:#}").contains("checkpoint-only worker is too old"),
            "{error:#}"
        );
        assert!(executor.exported.lock().unwrap().is_none());
        channels.shutdown.shutdown().await.unwrap();
        return;
    }
    assert!(
        format!("{error:#}").contains("no target is provisioned for this test"),
        "{error:#}"
    );
    let spec = executor
        .exported
        .lock()
        .unwrap()
        .take()
        .expect("the checkpoint never reached its export");
    spec.canonical_session.validate().unwrap();
    assert_eq!(
        spec.canonical_session.pending_prompt_context.as_deref(),
        Some(pending_context.as_str())
    );
    let notices = executor.notices.lock().unwrap().clone();
    if protocol < reads_queue {
        assert!(
            spec.canonical_session.queued_prompts.is_empty(),
            "a wake this worker cannot report was archived: {:?}",
            spec.canonical_session.queued_prompts
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.starts_with("1 queued message turn could not be saved")),
            "the unsaved wake went unreported: {notices:?}"
        );
    } else {
        let [queued] = spec.canonical_session.queued_prompts.as_slice() else {
            panic!(
                "expected only the wake's continuation in the archived queue: {:?}",
                spec.canonical_session.queued_prompts
            );
        };
        assert!(mj_core::continuation::is_restart_continuation(
            &queued.command_id
        ));
        assert!(mj_core::continuation::carries_mailbox_wake(
            &queued.command_id
        ));
        assert_eq!(
            queued.content,
            wake_prompt
                .iter()
                .map(|block| serde_json::to_value(block).unwrap())
                .collect::<Vec<_>>()
        );
        assert!(notices.is_empty(), "{notices:?}");
        // Restored, the wake shows and starts as a live one: no payload.
        assert!(
            mj_transcript::projection::materialized_queued_prompts_from_canonical(
                &spec.canonical_session.queued_prompts
            )[0]
            .content
            .is_empty()
        );
    }
    channels.shutdown.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_close_latch_reuses_an_unchanged_archive_and_exports_after_new_content() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(REUSE_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_close_latch_reuses_an_unchanged_archive_and_exports_after_new_content",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(REUSE_TEST_CHILD, "1")
            // Longer than the normal checkpoint barrier deadline. The
            // controller must wait for startup rather than restart it.
            .env(LATCH_RELAY_STARTUP_DELAY_MS, "31000")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    // A latch that never returns would hang the suite instead of failing
    // it, so turn a stall into a hard error.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the reuse checkpoint never finished its latch");
        std::process::exit(101);
    });

    #[derive(Default)]
    struct RecordingExecutor {
        purposes: std::sync::Mutex<Vec<String>>,
        active_stages: std::sync::Mutex<Vec<ProvisionStage>>,
        stage_events: std::sync::Mutex<Vec<(ProvisionStage, bool)>>,
        observed_stages: std::sync::Mutex<Vec<(String, Vec<ProvisionStage>)>>,
        exported_mailbox: std::sync::Mutex<Option<Option<mj_core::archive::CanonicalMailbox>>>,
        notices: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingExecutor {
        fn refused(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            self.observed_stages.lock().unwrap().push((
                command.purpose.clone(),
                self.active_stages.lock().unwrap().clone(),
            ));
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"no target is provisioned for this test".to_vec(),
            })
        }

        fn purposes(&self) -> Vec<String> {
            self.purposes.lock().unwrap().clone()
        }

        fn observed_stages(&self) -> Vec<(String, Vec<ProvisionStage>)> {
            self.observed_stages.lock().unwrap().clone()
        }

        fn stage_events(&self) -> Vec<(ProvisionStage, bool)> {
            self.stage_events.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for RecordingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.refused(command)
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            if command.purpose == "export target checkpoint" {
                let spec: CheckpointExportSpec = serde_json::from_reader(input)?;
                *self.exported_mailbox.lock().unwrap() = Some(spec.canonical_session.mailbox);
            }
            self.refused(command)
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }

        fn stage_started(&self, stage: ProvisionStage) {
            self.active_stages.lock().unwrap().push(stage.clone());
            self.stage_events.lock().unwrap().push((stage, true));
        }

        fn stage_finished(&self, stage: ProvisionStage) {
            let mut active = self.active_stages.lock().unwrap();
            let position = active
                .iter()
                .position(|active_stage| *active_stage == stage)
                .expect("stage finished without a matching start");
            active.remove(position);
            self.stage_events.lock().unwrap().push((stage, false));
        }
    }

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let relay_root = data_directory.join("relay");
    let profile_home = data_directory.join("profile");
    let archive_directory = data_directory.join("archives");
    for directory in [&relay_root, &profile_home, &archive_directory] {
        std::fs::create_dir_all(directory).unwrap();
    }
    if std::env::var_os(LATCH_CHECKPOINT_ONLY).is_some() {
        let mut seed =
            mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0")
                .unwrap();
        seed.record_observation(mj_core::relay::RelayObservation::SessionOpened {
            native_session_id: "native-session".into(),
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
            resumed: true,
        })
        .unwrap();
        seed.record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    }
    // A live relay synchronizes its empty goal before configuring the session.
    // That state survives configuration updates and belongs in its archive.
    let mut input = crate::controller::test_support::checkpoint_archive_input(
        LATCH_RELAY_SESSION,
        2,
        Vec::new(),
        Vec::new(),
    );
    if std::env::var_os(LATCH_CHECKPOINT_ONLY).is_none() {
        input.canonical_session.session.configuration.goal = Some(Box::new(
            serde_json::from_value(serde_json::json!({
                "known": true,
                "execution": {"version": 1, "status": "idle"}
            }))
            .unwrap(),
        ));
    }
    let checkpoint = crate::controller::test_support::write_checkpoint_archive_input(
        &archive_directory,
        LATCH_RELAY_SESSION,
        &input,
    );

    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target_template_id = "removed-local".into();
    session.target_runtime = Some((&TargetTemplate::LocalBare).into());
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: data_directory.join("workers").join(LATCH_RELAY_SESSION),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
    crate::database::save_session(&session).unwrap();

    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                profile_home,
            )
        },
    );
    // Checkpoint export must continue after its original template is removed.
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: Some("example/project".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
                mbx_namespace: None,
            }],
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [(LATCH_RELAY_SESSION.into(), LATCH_RELAY_SESSION.into())]
                .into_iter()
                .collect(),
            sessions: [(LATCH_RELAY_SESSION.into(), session)]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };

    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            &relay_root,
            None,
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();

    let executor = RecordingExecutor::default();
    let latched = controller
        .checkpoint_session_latched_for_operation_with_delivery(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::HoldThroughClose,
            CheckpointExportPolicy::ReuseUnchangedArchive,
            CheckpointDelivery::DownloadToDaemon,
            None,
            None,
            &mj_core::suspend_consent::CaptureConsent::default(),
        )
        .await
        .unwrap();

    assert!(
        executor.purposes().is_empty(),
        "an unchanged session exported an archive anyway: {:?}",
        executor.purposes()
    );
    assert_eq!(latched.artifact.metadata, checkpoint);
    assert!(checkpoint.archive_path.exists());

    // The cursor close seals is ahead of the reused archive by this
    // checkpoint's own bookkeeping.
    assert!(latched.cursor.ordinal > checkpoint.event_frontier);
    let cursor = latched.cursor.clone();
    let mut latched = latched;
    let barrier_command_id = latched.barrier_command_id.clone();
    complete_held_barrier(&mut latched.relay, &barrier_command_id).await;
    // Ending the lease hands the connection back to the session actor.
    drop(latched);
    wait_until_the_actor_serves_again(&handle).await;

    if std::env::var_os(LATCH_CHECKPOINT_ONLY).is_some() {
        let snapshot = handle.view().snapshot.unwrap();
        assert!(snapshot.operational.checkpoint_only);
        assert!(!snapshot.operational.native_session_is_ready());
        assert_eq!(
            verify_archive_streaming(&checkpoint.archive_path)
                .unwrap()
                .manifest
                .session
                .native_session_id,
            "native-session"
        );
        channels.shutdown.shutdown().await.unwrap();
        return;
    }

    // An ordinary recovery copy during a turn must defer before it
    // journals BeginCheckpoint, so no disconnect-cancellation message is
    // produced for a routine busy observation.
    handle
        .submit(
            new_command_id("busy-prompt").unwrap(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("keep working"))],
            },
        )
        .await
        .unwrap();
    let mut connection = handle
        .lease_connection(&crate::controller::test_support::session_admission(
            handle.session_id(),
        ))
        .await
        .unwrap();
    let before = connection.connection_mut().sync().await.unwrap();
    assert_eq!(before.operational.execution, RelayExecutionState::Running);
    connection.release();
    let deferred = controller
        .checkpoint_session_latched_for_operation_with_delivery(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::ReleaseAfterLatch,
            CheckpointExportPolicy::ReuseUnchangedArchive,
            CheckpointDelivery::DownloadToDaemon,
            None,
            None,
            &mj_core::suspend_consent::CaptureConsent::default(),
        )
        .await;
    assert!(
        matches!(deferred, Err(ref error) if error.downcast_ref::<CheckpointDeferred>().is_some())
    );
    wait_until_the_actor_serves_again(&handle).await;
    let mut connection = handle
        .lease_connection(&crate::controller::test_support::session_admission(
            handle.session_id(),
        ))
        .await
        .unwrap();
    let after = connection.connection_mut().sync().await.unwrap();
    assert_eq!(after.operational.execution, RelayExecutionState::Running);
    assert!(after.operational.checkpoint_barrier.is_none());
    let journal = std::fs::read_to_string(relay_root.join("relay-journal/active.jsonl")).unwrap();
    for line in journal.lines() {
        let event: mj_core::relay::RelayEvent = serde_json::from_str(line).unwrap();
        if event.ordinal > before.operational.latest_ordinal {
            assert!(
                !matches!(
                    event.observation,
                    mj_core::relay::RelayObservation::CommandQueued {
                        command: RelayCommand::BeginCheckpoint { .. },
                        ..
                    } | mj_core::relay::RelayObservation::CommandInterrupted {
                        command: mj_core::relay::RelayCommandKind::BeginCheckpoint,
                        ..
                    }
                ),
                "busy deferral journaled checkpoint activity: {event:?}"
            );
        }
    }
    connection.release();
    handle
        .submit(
            new_command_id("finish-busy-prompt").unwrap(),
            RelayCommand::CancelTurn,
        )
        .await
        .unwrap();
    handle.sync_now().await.unwrap();

    // Real session content, and the same policy has to export again. This
    // stand-in speaks a protocol older than checkpoint mail, so a message it
    // accepted but has not handed to the agent cannot be saved; the export
    // says nothing about it and the person is told (#1616).
    let waiting = mj_core::mailbox::MailboxEvent {
        key: "waiting-message".into(),
        source: "parent".into(),
        wake: false,
        created_at_ms: 1,
        body: mj_core::mailbox::MailboxEventBody::PlainText {
            text: "read this with your next prompt".into(),
        },
    };
    crate::database::enqueue_mailbox_event(
        &waiting.key,
        LATCH_RELAY_SESSION,
        &serde_json::to_string(&waiting).unwrap(),
        false,
        false,
    )
    .unwrap();
    assert!(matches!(
        handle.deliver_mailbox_event(waiting.clone()).await.unwrap(),
        crate::mailbox_outbox::MailboxEventDelivery::Accepted(_)
    ));
    handle
        .submit(
            new_command_id("resume-notice").unwrap(),
            RelayCommand::RecordNotice {
                text: "the session changed".into(),
            },
        )
        .await
        .unwrap();
    for attempt in 0.. {
        handle.sync_now().await.unwrap();
        let materialized = handle.view().snapshot.map(|snapshot| snapshot.materialized);
        if materialized.is_some_and(|materialized| {
            materialized.applied_event_ordinal > cursor.ordinal
                && !materialized.transcript.is_empty()
        }) {
            break;
        }
        assert!(attempt < 200, "the notice never reached the projection");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let changed = controller
        .checkpoint_session_latched_for_operation_with_delivery(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::HoldThroughClose,
            CheckpointExportPolicy::ReuseUnchangedArchive,
            CheckpointDelivery::DownloadToDaemon,
            None,
            None,
            &mj_core::suspend_consent::CaptureConsent::default(),
        )
        .await;
    let Err(error) = changed else {
        panic!("a changed session reused its installed archive");
    };

    assert!(
        executor
            .purposes()
            .contains(&"export target checkpoint".to_owned()),
        "a changed session skipped its export: {:?}",
        executor.purposes()
    );
    assert!(
        format!("{error:#}").contains("no target is provisioned for this test"),
        "{error:#}"
    );
    assert_eq!(
        executor
            .exported_mailbox
            .lock()
            .unwrap()
            .clone()
            .expect("the export spec was not read"),
        None
    );
    let notices = executor.notices.lock().unwrap().clone();
    assert!(
        notices
            .iter()
            .any(|notice| notice.starts_with("1 accepted message not yet delivered")),
        "the unsaved message went unreported: {notices:?}"
    );
    assert!(
        executor.observed_stages().iter().any(|(purpose, stages)| {
            purpose == "export target checkpoint" && stages.contains(&ProvisionStage::RecoveryCopy)
        }),
        "close checkpoint export did not run inside RecoveryCopy: {:?}",
        executor.observed_stages()
    );
    assert_eq!(
        executor
            .stage_events()
            .into_iter()
            .filter(|(stage, _)| stage == &ProvisionStage::RecoveryCopy)
            .collect::<Vec<_>>(),
        vec![
            (ProvisionStage::RecoveryCopy, true),
            (ProvisionStage::RecoveryCopy, false)
        ]
    );
    assert!(executor.active_stages.lock().unwrap().is_empty());
    assert!(checkpoint.archive_path.exists());
}
/// A routine checkpoint copies the session's native state into a stage on its
/// target before it asks for the barrier, so the barrier is held only while
/// that stage is refreshed. When the barrier then defers because the agent is
/// working, the stage is never packed, and packing is what removes it. That
/// left a 260 MB directory behind on every deferred attempt until the disk
/// filled. Every exit that does not hand the stage to the pack step removes
/// it.
// Hard-won: 7f0d41b9de8d: deferred recovery copies leaked roughly 260 MB of target stages on repeated attempts
#[cfg(unix)]
#[tokio::test]
async fn a_deferred_routine_checkpoint_leaves_no_stage_on_its_target() {
    if std::env::var_os(STAGE_CLEANUP_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_deferred_routine_checkpoint_leaves_no_stage_on_its_target",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(STAGE_CLEANUP_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("the deferred checkpoint never returned");
        std::process::exit(101);
    });

    /// Does the target's side of a prestage here: it seals a stage where the
    /// spec says, as the worker's capture does. Every other command runs for
    /// real, so a removal really removes.
    #[derive(Default)]
    struct StagingExecutor {
        purposes: std::sync::Mutex<Vec<String>>,
    }

    impl StagingExecutor {
        fn purposes(&self) -> Vec<String> {
            self.purposes.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for StagingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            ProcessExecutor.execute(command)
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            let spec: CheckpointCaptureSpec = serde_json::from_reader(input)?;
            // The prestage is a capture that starts a fresh generation; the
            // barrier's capture refreshes it and must not run in this test.
            anyhow::ensure!(
                command.purpose == "capture target checkpoint" && !spec.refresh_existing,
                "this test stages only the prestage, not {}",
                command.purpose
            );
            let native = spec.stage_path.join("native");
            std::fs::create_dir_all(&native)?;
            std::fs::write(native.join("00000000"), vec![7_u8; 256 * 1024])?;
            std::fs::write(spec.stage_path.join("stage.json"), b"{}")?;
            Ok(CommandOutput {
                status: 0,
                stdout: serde_json::to_vec(&CapturedCheckpoint {
                    omitted_reports: Default::default(),
                    stage_path: spec.stage_path,
                    native_bytes: 256 * 1024,
                    repository_bytes: 0,
                    reused_native: false,
                })?,
                stderr: Vec::new(),
            })
        }
    }

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let relay_root = data_directory.join("relay");
    let profile_home = data_directory.join("profile");
    let worker_root = data_directory.join("workers").join(LATCH_RELAY_SESSION);
    for directory in [&relay_root, &profile_home, &worker_root] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target_template_id = "removed-local".into();
    session.target_runtime = Some((&TargetTemplate::LocalBare).into());
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    }));
    crate::database::save_session(&session).unwrap();
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                profile_home,
            )
        },
    );
    config.bundles.insert(
        "project".into(),
        ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![ProjectRepository {
                id: "project".into(),
                github: Some("example/project".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
                mbx_namespace: None,
            }],
        },
    );
    let controller = Controller {
        config,
        state: State {
            environment_owners: [(LATCH_RELAY_SESSION.into(), LATCH_RELAY_SESSION.into())]
                .into_iter()
                .collect(),
            sessions: [(LATCH_RELAY_SESSION.into(), session)]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };

    // The stand-in relay starts with a prompt running, so the checkpoint's
    // admission finds the agent working after the prestage has run.
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            &relay_root,
            None,
            ReleaseSupport::Supported,
            true,
        )])
        .unwrap();
    channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();

    let executor = StagingExecutor::default();
    let deferred = controller
        .checkpoint_session_latched_for_operation_with_delivery(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::ReleaseAfterLatch,
            CheckpointExportPolicy::Always,
            CheckpointDelivery::DownloadToDaemon,
            None,
            None,
            &mj_core::suspend_consent::CaptureConsent::default(),
        )
        .await;
    let Err(error) = deferred else {
        panic!("a working session was checkpointed");
    };
    assert!(checkpoint_was_deferred(&error), "{error:#}");
    assert!(
        executor
            .purposes()
            .contains(&"capture target checkpoint".to_owned()),
        "the checkpoint deferred before it staged anything, so this proves nothing: {:?}",
        executor.purposes()
    );
    let left = std::fs::read_dir(&worker_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        left.is_empty(),
        "the deferred checkpoint left {left:?} in its worker root"
    );
    channels.shutdown.shutdown().await.unwrap();
}
#[cfg(unix)]
fn relay_starts(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}
#[test]
fn checkpoint_persistence_rollback_restores_memory_and_reports_both_failures() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let previous = checkpoint_test_session(session_id);
    let mut changed = previous.clone();
    changed.state = SessionState::Closing;
    changed.last_checkpoint_error = Some("partially installed checkpoint".into());
    let mut state = State::default();
    state.sessions.insert(session_id.into(), changed);

    let error = restore_session_after_persistence_failure(
        &mut state,
        session_id,
        &previous,
        anyhow::anyhow!("verified checkpoint persistence failed"),
        |record| {
            assert_eq!(record, &previous);
            Err(anyhow::anyhow!("rollback database write failed"))
        },
    );

    assert_eq!(state.sessions.get(session_id), Some(&previous));
    let detail = format!("{error:#}");
    assert!(detail.contains("verified checkpoint persistence failed"));
    assert!(detail.contains("rollback database write failed"));
}
#[test]
fn installed_checkpoint_gate_reopens_and_checks_sha() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    verify_installed_checkpoint_gate(session_id, &checkpoint).unwrap();

    let mut wrong_sha = checkpoint.clone();
    wrong_sha.sha256 = "b".repeat(64);
    assert!(
        verify_installed_checkpoint_gate(session_id, &wrong_sha)
            .unwrap_err()
            .to_string()
            .contains("SHA changed")
    );
    std::fs::write(
        &checkpoint.archive_path,
        b"changed after first verification",
    )
    .unwrap();
    assert!(
        format!(
            "{:#}",
            verify_installed_checkpoint_gate(session_id, &checkpoint).unwrap_err()
        )
        .contains("installed checkpoint SHA changed")
    );
}
#[test]
fn an_installed_archive_is_reused_when_only_relay_bookkeeping_moved() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let archived = verify_archive_streaming(&checkpoint.archive_path)
        .unwrap()
        .canonical_session;

    // What a checkpoint taken seconds later latches on an idle session:
    // the frontier and the activity watermark moved, the content did not.
    let mut latched = archived.clone();
    latched.event_frontier += 6;
    latched.event_frontier_digest = "b".repeat(64);
    latched.session.last_activity_at_ms = Some(9_999);

    let artifact = reusable_installed_checkpoint(
        session_id,
        Some(&checkpoint),
        "native-session",
        latched.event_frontier,
        &latched,
    )
    .expect("an unchanged session reuses its installed archive");

    assert_eq!(artifact.metadata, checkpoint);
    assert_eq!(artifact.native_session_id, "native-session");
    assert_eq!(
        artifact.event_frontier_digest,
        archived.event_frontier_digest
    );
    // The reused archive is still the gate close destroys through.
    verify_installed_checkpoint_gate(session_id, &artifact.metadata).unwrap();
}
// Hard-won: pending handoff installation does not advance the archive frontier.
#[test]
fn archive_reuse_falls_back_to_a_full_export_for_anything_but_bookkeeping() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "0123456789abcdef0123456789abcdef";
    let checkpoint = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let archived = verify_archive_streaming(&checkpoint.archive_path)
        .unwrap()
        .canonical_session;
    let mut latched = archived.clone();
    latched.event_frontier += 6;
    let reuse = |installed: Option<&CheckpointMetadata>,
                 ordinal: u64,
                 session: &CanonicalSessionSnapshot| {
        reusable_installed_checkpoint(session_id, installed, "native-session", ordinal, session)
    };

    assert!(reuse(None, latched.event_frontier, &latched).is_none());

    let mut with_new_content = latched.clone();
    with_new_content.transcript.push(CanonicalTranscriptItem {
        stable_id: "system:notice:notice-1".into(),
        position: latched.event_frontier,
        latest_content_event_ordinal: None,
        created_at_ms: 2_000,
        last_changed_at_ms: 2_000,
        body: CanonicalTranscriptBody::System {
            text: "resumed".into(),
        },
    });
    assert!(reuse(Some(&checkpoint), latched.event_frontier, &with_new_content).is_none());

    let mut with_pending_context = latched.clone();
    with_pending_context.pending_prompt_context = Some("hidden context".repeat(8_000));
    assert!(
        reuse(
            Some(&checkpoint),
            latched.event_frontier,
            &with_pending_context
        )
        .is_none()
    );

    // An archive the latch has not reached yet cannot describe the session.
    assert!(reuse(Some(&checkpoint), checkpoint.event_frontier - 1, &latched).is_none());

    let mut wrong_sha = checkpoint.clone();
    wrong_sha.sha256 = "b".repeat(64);
    assert!(reuse(Some(&wrong_sha), latched.event_frontier, &latched).is_none());

    let another_session =
        write_checkpoint_gate_archive(directory.path(), "1123456789abcdef0123456789abcdef", 7);
    assert!(reuse(Some(&another_session), latched.event_frontier, &latched).is_none());

    std::fs::write(&checkpoint.archive_path, b"not an archive any more").unwrap();
    assert!(reuse(Some(&checkpoint), latched.event_frontier, &latched).is_none());
}
#[cfg(unix)]
#[tokio::test]
async fn workspace_lease_blocks_prompts_and_releases_without_advancing_recovery() {
    if std::env::var_os(LATCH_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::workspace_lease_blocks_prompts_and_releases_without_advancing_recovery",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let mut command = crate::targets::CommandSpec::new(
            std::env::current_exe().unwrap().to_string_lossy(),
            ["--exact", &name, "--nocapture"],
        );
        command.env.insert(LATCH_TEST_CHILD.into(), "1".into());
        command.env.insert(
            "MJ_DATA_DIR".into(),
            directory.path().to_string_lossy().into(),
        );
        let result =
            crate::targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(60))
                .execute(&command)
                .unwrap();
        assert_eq!(
            result.status,
            0,
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("1 passed"),
            "child did not run its test"
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let (_channels, handle, mut relay, barrier, _cursor) =
        latch_a_live_checkpoint(root.path(), None, ReleaseSupport::Supported, false).await;
    relay
        .connection_mut()
        .submit(
            new_command_id("release-initial").unwrap(),
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: barrier,
            },
        )
        .await
        .unwrap();
    relay.release();
    wait_until_the_actor_serves_again(&handle).await;
    let before = handle
        .view()
        .snapshot
        .unwrap()
        .operational
        .recovery_floor_ordinal;
    let mut workspace = IdleWorkspaceLease::acquire(
        &crate::controller::test_support::session_admission(handle.session_id()),
        &handle,
        HarnessKind::Codex,
    )
    .await
    .unwrap();
    workspace.verify().await.unwrap();
    drop(workspace);
    wait_until_the_actor_serves_again(&handle).await;
    assert!(
        handle
            .view()
            .snapshot
            .unwrap()
            .operational
            .checkpoint_barrier
            .is_none()
    );
    let mut workspace = IdleWorkspaceLease::acquire(
        &crate::controller::test_support::session_admission(handle.session_id()),
        &handle,
        HarnessKind::Codex,
    )
    .await
    .unwrap();
    let submitting = handle.clone();
    let mut prompt = mj_core::runtime::spawn(async move {
        submitting
            .submit(
                new_command_id("after-write").unwrap(),
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("go"))],
                },
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut prompt)
            .await
            .is_err(),
        "prompt must wait for the workspace owner"
    );
    workspace.verify().await.unwrap();
    workspace.release().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    wait_until_the_actor_serves_again(&handle).await;
    let after = handle.view().snapshot.unwrap();
    assert!(after.operational.checkpoint_barrier.is_none());
    assert_eq!(after.operational.recovery_floor_ordinal, before);
    assert!(
        IdleWorkspaceLease::acquire(
            &crate::controller::test_support::session_admission(handle.session_id()),
            &handle,
            HarnessKind::Codex
        )
        .await
        .is_err(),
        "a queued or running prompt must prevent file injection"
    );
}

#[cfg(unix)]
const IN_PLACE_CLOSE_CHILD: &str = "MJ_TEST_IN_PLACE_CLOSE_CHILD";

/// A cross-target Move captures its snapshot onto the source target without
/// overwriting the session's existing suspend checkpoint or tearing down the
/// source environment before `DestinationOwns` commits.
#[cfg(unix)]
#[tokio::test]
async fn a_cross_target_move_capture_keeps_the_source_target_and_preserves_session_checkpoint() {
    if std::env::var_os(IN_PLACE_CLOSE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::a_cross_target_move_capture_keeps_the_source_target_and_preserves_session_checkpoint",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(name)
            .env(IN_PLACE_CLOSE_CHILD, "1")
            // Checkpoint-only mode advances the lifecycle commands itself, so
            // the sealed Close reaches `Closed` without an ACP harness.
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_DATA_DIR", directory.path().join("data"))
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();

    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let relay_root = data_directory.join("workers").join(LATCH_RELAY_SESSION);
    let profile_home = data_directory.join("profile");
    let archive_directory = data_directory.join("archives");
    for directory in [&relay_root, &profile_home, &archive_directory] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::create_dir_all(relay_root.join("profile")).unwrap();
    mj_core::test_hooks::install_fake_command(
        &relay_root,
        "hel",
        &format!(
            "case \"${{2:-}}\" in proxy) {} ;; esac\nexit 0\n",
            latch_relay_proxy_command(&relay_root)
        ),
    );
    // The archive covers exactly the two observations seeded below, so the
    // close latch reuses it instead of exporting a new one.
    let mut seed =
        mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    seed.record_observation(mj_core::relay::RelayObservation::SessionOpened {
        native_session_id: "native-session".into(),
        native_continuity_lost: false,
        replaced_unused_native_session_id: None,
        resumed: true,
    })
    .unwrap();
    seed.record_observation(mj_core::relay::RelayObservation::SessionConfigured {
        config_options: Vec::new(),
    })
    .unwrap();
    drop(seed);
    let checkpoint = write_checkpoint_gate_archive(&archive_directory, LATCH_RELAY_SESSION, 2);

    let repository = crate::controller::test_support::committed_repository();
    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target_template_id = "local".into();
    session.project_directory = Some(repository.path().to_owned());
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: data_directory.join("workers").join(LATCH_RELAY_SESSION),
    }));
    session.checkpoint = Some(Box::new(checkpoint.clone()));
    crate::database::save_session(&session).unwrap();

    // `validate_move_checkpoint` re-reads the configuration from disk and
    // compares its fingerprint, so the controller's config has to be the
    // persisted one.
    let (config, ()) = Config::update(|config| {
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                ..mj_core::config::HarnessProfile::new(
                    mj_core::config::HarnessKind::Codex,
                    profile_home.clone(),
                )
            },
        );
        config
            .targets
            .insert("local".into(), TargetTemplate::LocalBare);
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: Some("example/project".into()),
                    local: None,
                    destination: "project".into(),
                    git_ref: None,
                    mbx_namespace: None,
                }],
            },
        );
        Ok(())
    })
    .unwrap();

    let mut controller = Controller {
        config,
        state: State {
            environment_owners: [(LATCH_RELAY_SESSION.into(), LATCH_RELAY_SESSION.into())]
                .into_iter()
                .collect(),
            sessions: [(LATCH_RELAY_SESSION.into(), session.clone())]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };

    let selection = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        consent: Default::default(),
        skip_keep_paths: Vec::new(),
        clear_resource_allocation: false,
        session_id: LATCH_RELAY_SESSION.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("local".into()),
        additional_mounts: Some(Vec::new()),
        resource_allocation: None,
    };
    let mut operation = mj_core::state::MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        transfer_route: None,
        consent: Default::default(),
        skip_keep_paths: Vec::new(),
        granted_keys: Vec::new(),
        granted_ec2_rules: Vec::new(),
        in_place_expected_store_id: None,
        in_place_continuation_command_id: None,
        in_place_held_command_ids: Vec::new(),
        in_place_harness_restarted: false,
        deferred_environment_changes: Vec::new(),
        source_snapshot: None,
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-cross-target-capture".into(),
        selection: selection.clone(),
        source_profile_id: "codex".into(),
        source_target_template_id: "local".into(),
        source_target: session.target.clone().map(|v| *v),
        source_native_session_id: session.native_session_id.clone(),
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: controller
            .move_configuration_fingerprint(&selection)
            .unwrap(),
        checkpoint: None,
        recovery_session: None,
        queue: mj_core::state::ResumeQueueDisposition::Discard,
        phase: mj_core::state::MovePhase::Capturing,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-08-14T12:00:00Z".into(),
        updated_at: "2026-08-14T12:00:00Z".into(),
        error: None,
    };
    crate::database::save_move_operation(&operation).unwrap();

    #[derive(Default)]
    struct RecordingExecutor {
        purposes: std::sync::Mutex<Vec<String>>,
    }
    impl CommandExecutor for RecordingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            ProcessExecutor.execute(command)
        }
        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            mut input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            anyhow::ensure!(
                command.purpose == "export target checkpoint",
                "unexpected command"
            );
            let checkpoint = mj_worker::checkpoint::export_from_spec_reader(&mut input)?;
            Ok(CommandOutput {
                status: 0,
                stdout: serde_json::to_vec(&checkpoint)?,
                stderr: Vec::new(),
            })
        }
    }

    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            &relay_root,
            None,
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    let executor = RecordingExecutor::default();
    let deferred = controller
        .suspend_session_for_move(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &executor,
            &channels.control,
            &mut operation,
            None,
            crate::controller::lifecycle::SourceTargetDisposition::RetainForInPlaceSwap,
            crate::controller::move_session::MoveSourceRelay::default(),
        )
        .await
        .unwrap();
    channels.shutdown.shutdown().await.unwrap();

    assert!(
        !deferred,
        "a retained target has no deferred storage cleanup"
    );
    let sealed = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(sealed.state, SessionState::Closing);
    assert_eq!(sealed.checkpoint.as_deref(), Some(&checkpoint));
    assert_eq!(sealed.target, session.target);
    assert!(
        operation.checkpoint.is_none(),
        "cross-target Move must not install a daemon checkpoint row"
    );
    let snapshot = operation
        .source_snapshot
        .as_ref()
        .expect("cross-target Move records its source-target snapshot");
    assert!(
        snapshot.archive_path.exists(),
        "source-target snapshot must remain on source for direct streaming"
    );
    assert!(!snapshot.sha256.is_empty());
    // The durable record has to agree: recovery reads it, not this process.
    let persisted = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(persisted.state, SessionState::Closing);
    assert_eq!(persisted.target, session.target);
    assert_eq!(persisted.checkpoint.as_deref(), Some(&checkpoint));

    let purposes = executor.purposes.lock().unwrap().clone();
    for purpose in &purposes {
        let lowered = purpose.to_lowercase();
        assert!(
            !lowered.contains("remove exact")
                && !lowered.contains("delete")
                && !lowered.contains("retire"),
            "cross-target Move capture must not tear down the source before DestinationOwns, but it ran {purpose:?} \
             (all: {purposes:?})"
        );
    }
    assert!(
        !purposes.iter().any(|purpose| purpose.contains("podman")),
        "no container command ran at all: {purposes:?}"
    );
}

/// Run `test_name` in its own process with its own store, behind a scripted
/// worker that makes `late_write` just before it reads the Close. Returns
/// true in that child, where the caller runs its body.
#[cfg(unix)]
fn in_close_cut_child(test_name: &str, late_write: &str) -> bool {
    if std::env::var_os(CLOSE_CUT_TEST_CHILD).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    let test_name = format!(
        "{}::{test_name}",
        module_path!()
            .strip_prefix("mj_controller::")
            .unwrap_or(module_path!())
    );
    IsolatedTest::new(test_name)
        .env(CLOSE_CUT_TEST_CHILD, "1")
        .env(LATCH_RELAY_JOURNALS_BEFORE_CLOSE, late_write)
        .env("MJ_DATA_DIR", directory.path())
        .run();
    false
}

/// Every target command succeeds. A close behind [`close_cut_controller`]
/// reuses the installed archive, so the only commands are the local target's
/// teardown.
#[cfg(unix)]
struct SucceedingExecutor;

#[cfg(unix)]
impl CommandExecutor for SucceedingExecutor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
        Ok(CommandOutput {
            status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        _input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        anyhow::bail!("unexpected streamed command {:?}", command.purpose)
    }
}

#[cfg(unix)]
#[derive(Default)]
struct RestartCheckpointExecutor {
    purposes: std::sync::Mutex<Vec<String>>,
    archive_directory: PathBuf,
}

#[cfg(unix)]
impl RestartCheckpointExecutor {
    fn purposes(&self) -> Vec<String> {
        self.purposes.lock().unwrap().clone()
    }
}

#[cfg(unix)]
impl CommandExecutor for RestartCheckpointExecutor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.purposes.lock().unwrap().push(command.purpose.clone());
        targets::ProcessExecutor.execute(command)
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        self.purposes.lock().unwrap().push(command.purpose.clone());
        anyhow::ensure!(
            command.purpose == "export target checkpoint",
            "unexpected streamed command {:?}",
            command.purpose
        );
        let spec: CheckpointExportSpec = serde_json::from_reader(input)?;
        let canonical_session = spec.canonical_session.clone();
        let mut archive = crate::controller::test_support::checkpoint_archive_input(
            &spec.session.id,
            canonical_session.event_frontier,
            Vec::new(),
            Vec::new(),
        );
        archive.session = spec.session;
        archive.target = spec.target;
        archive.bundle = spec.bundle;
        archive.canonical_session = canonical_session.clone();
        let metadata = crate::controller::test_support::write_checkpoint_archive_input(
            &self.archive_directory,
            &archive.session.id,
            &archive,
        );
        std::fs::create_dir_all(spec.output_path.parent().context("archive output parent")?)?;
        std::fs::copy(&metadata.archive_path, &spec.output_path)?;
        let checkpoint = mj_checkpoint::checkpoint::TargetCheckpoint {
            path: spec.output_path,
            sha256: metadata.sha256,
            event_frontier: canonical_session.event_frontier,
            event_frontier_digest: canonical_session.event_frontier_digest,
            timings: None,
            manifest: None,
        };
        Ok(CommandOutput {
            status: 0,
            stdout: serde_json::to_vec(&checkpoint)?,
            stderr: Vec::new(),
        })
    }
}

/// I2-3 (test-and-fix campaign 2026-09-29): a suspend issued right after a
/// turn ended failed with "close does not match the current checkpoint cut".
///
/// The daemon took the cut from the barrier's ready cursor, but the worker
/// kept journaling its own output under that barrier (the completed-turn
/// classifier's answer, the harness's notifications), and its Close requires
/// the frontier to equal the cut exactly. One such write between the daemon's
/// last look and its Close made the worker refuse.
///
/// The worker now owns the cut: while a barrier is ready, output that changes
/// no work waits, and a relay sealed at the cut drops it. The stand-in worker
/// here makes that write through the real relay just before it reads the
/// Close, the window the real session hit about half a second after its first
/// turn ended, and the suspend seals and stops the session.
// Hard-won: 634af0a367a2: I2-3 worker-owned journal writes moved the daemon cut and made suspend fail
#[cfg(unix)]
#[tokio::test]
async fn a_suspend_seals_when_the_worker_journals_after_the_close_cut() {
    if !in_close_cut_child(
        "a_suspend_seals_when_the_worker_journals_after_the_close_cut",
        LATE_HARNESS_NOTIFICATION,
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, channels, _handle) = close_cut_controller(SessionState::Running).await;
    let suspended = controller
        .suspend_session_managed_controlled(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &SucceedingExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
            crate::controller::lifecycle::SuspendSettlement::Suspend,
        )
        .await;
    channels.shutdown.shutdown().await.unwrap();

    let record = &controller.state.sessions[LATCH_RELAY_SESSION];
    if let Err(error) = suspended {
        panic!(
            "the suspend failed: {error:#}; the session was left {:?} with error {:?}",
            record.state, record.last_error
        );
    }
    assert_eq!(record.state, SessionState::Stopped);
    assert_eq!(record.last_error, None);
}

/// The container recreation an upgrade starts on its own may never cancel
/// accepted work. Over a running turn its checkpoint stands down before any
/// barrier is admitted: the turn keeps running (the stand-in worker would
/// clear it on `CancelTurn`), the session stays live, and nothing is
/// recorded as a failure.
#[cfg(unix)]
#[tokio::test]
async fn an_automatic_restart_stands_down_instead_of_cancelling_a_running_turn() {
    if !in_close_cut_child(
        "an_automatic_restart_stands_down_instead_of_cancelling_a_running_turn",
        LATE_HARNESS_NOTIFICATION,
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, channels, handle) =
        close_cut_controller_with(SessionState::Running, true).await;
    wait_until_the_actor_serves_again(&handle).await;
    let error = controller
        .suspend_session_for_restart(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &SucceedingExecutor,
            &channels.control,
            None,
            true,
        )
        .await
        .expect_err("an automatic restart must not close over a running turn");
    assert!(
        crate::controller::checkpoint_was_deferred(&error),
        "the stand-down is a deferral, not a failure: {error:#}"
    );
    handle.sync_now().await.unwrap();
    let snapshot = handle.view().snapshot.expect("live relay view");
    assert_eq!(
        snapshot
            .operational
            .active_prompt
            .as_ref()
            .map(|prompt| prompt.command_id.as_str()),
        Some("seed-running-prompt"),
        "the running turn was interrupted"
    );
    assert!(snapshot.operational.checkpoint_barrier.is_none());
    let stored = crate::database::load_session_record(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    for record in [&controller.state.sessions[LATCH_RELAY_SESSION], &stored] {
        assert_eq!(record.state, SessionState::Running);
        assert_eq!(record.last_error, None);
        assert_eq!(record.last_checkpoint_error, None);
    }
    channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1326: an image upgrade stopped accepted work and destroyed its environment.
#[cfg(unix)]
#[tokio::test]
async fn a_queued_container_upgrade_yields_handoff_and_cancels_without_interrupting_work() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    if !in_close_cut_child(
        "a_queued_container_upgrade_yields_handoff_and_cancels_without_interrupting_work",
        LATE_HARNESS_NOTIFICATION,
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, channels, handle) =
        close_cut_controller_with(SessionState::Running, true).await;
    wait_until_the_actor_serves_again(&handle).await;
    let mut plan = targets::container_upgrade_test_plan(LATCH_RELAY_SESSION);
    let name = targets::resource_name(LATCH_RELAY_SESSION).unwrap();
    let record = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    record.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: name.clone(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: format!("{name}-workspace"),
        },
        borrowed_from: None,
    }));
    record.target_runtime = Some(
        (&TargetTemplate::LocalPodman {
            container: serde_json::from_value(serde_json::json!({})).unwrap(),
        })
            .into(),
    );
    crate::database::save_session(record).unwrap();
    let original = record.clone();
    struct BusyEngine {
        cancel: Arc<AtomicBool>,
        admitted: AtomicBool,
        commands: Mutex<Vec<String>>,
        name: String,
    }
    impl CommandExecutor for BusyEngine {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.purpose.clone());
            let stdout = match command.purpose.as_str() {
                "inspect retained session container" => serde_json::to_vec(&serde_json::json!([{
                    "Id": "source-container", "Name": self.name, "Image": "sha256:source", "ImageName": "agent-dev:2.51.0",
                    "Config": {"Image":"agent-dev:2.51.0", "Labels": {"dev.mj.managed":"true", "dev.mj.session":LATCH_RELAY_SESSION}},
                    "HostConfig": {}, "Mounts": [], "State": {"Running":true}
                }]))?,
                "probe Mjolnir worker daemon liveness" => b"alive\n".to_vec(),
                _ => anyhow::bail!(
                    "a pending upgrade tried to mutate its busy environment: {}",
                    command.purpose
                ),
            };
            Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }
        fn cancellation_requested(&self) -> bool {
            self.cancel.load(Ordering::Acquire)
        }
        fn begin_resumable_work(&self) -> Result<()> {
            self.admitted.store(false, Ordering::Release);
            Ok(())
        }
        fn end_resumable_work(&self, _: &'static str) -> Result<()> {
            self.admitted.store(true, Ordering::Release);
            Ok(())
        }
        fn stage_started(&self, stage: ProvisionStage) {
            if stage == ProvisionStage::WaitingForContainerUpgrade {
                assert!(
                    !self.admitted.load(Ordering::Acquire),
                    "waiting for idle held daemon handoff admission"
                );
                self.cancel.store(true, Ordering::Release);
            }
        }
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let intent = crate::database::begin_container_upgrade(
        LATCH_RELAY_SESSION,
        "upgrade-test",
        cancel.clone(),
        plan.clone(),
        None,
    )
    .unwrap();
    let engine = BusyEngine {
        cancel,
        admitted: AtomicBool::new(false),
        commands: Mutex::new(Vec::new()),
        name,
    };
    let error = controller
        .upgrade_container_worker(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &mut plan,
            false,
            &engine,
            &channels.control,
            || panic!("a busy worker cannot commit container replacement"),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    handle.sync_now().await.unwrap();
    let snapshot = handle.view().snapshot.unwrap();
    assert_eq!(
        snapshot
            .operational
            .active_prompt
            .as_ref()
            .map(|prompt| prompt.command_id.as_str()),
        Some("seed-running-prompt")
    );
    assert!(snapshot.operational.checkpoint_barrier.is_none());
    assert_eq!(
        crate::database::load_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(crate::database::load_session_restarts().unwrap()[0], intent);
    crate::database::request_session_restart_cancel(LATCH_RELAY_SESSION, "upgrade-test").unwrap();
    crate::database::recover_cancelled_session_restart(LATCH_RELAY_SESSION, "upgrade-test")
        .unwrap();
    assert!(crate::database::load_session_restarts().unwrap().is_empty());
    assert_eq!(
        crate::database::load_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap(),
        original
    );
    channels.shutdown.shutdown().await.unwrap();
}

/// A failed suspend can leave an `Error` record and no relay worker while its
/// target is still present. Restart must attach to the durable relay again,
/// export the target's current state, and seal it without cleaning the target.
// Hard-won: 8dd7f6384: omitting an empty stack let restore seed a cleared notification.
#[cfg(unix)]
#[tokio::test]
async fn restart_recaptures_an_error_session_after_its_worker_is_gone() {
    if !in_close_cut_child(
        "restart_recaptures_an_error_session_after_its_worker_is_gone",
        LATE_HARNESS_NOTIFICATION,
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, channels, previous_worker) =
        close_cut_controller(SessionState::Error).await;
    let data_directory = PathBuf::from(std::env::var_os("MJ_DATA_DIR").unwrap());
    let relay_root = data_directory.join("relay");
    let starts = data_directory.join("checkpoint-only-worker-starts");
    let archive_directory = data_directory.join("fresh-checkpoints");
    std::fs::create_dir_all(&archive_directory).unwrap();
    let record = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    record.last_error = Some("the previous suspend lost its worker".into());
    record.last_checkpoint_error = Some("worker process is gone".into());
    let target = record.target.clone().expect("retained target");
    let old_checkpoint = record.checkpoint.clone().expect("previous checkpoint");
    crate::database::begin_session_restart(
        LATCH_RELAY_SESSION,
        "restart-recapture",
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        false,
    )
    .unwrap();
    let worker_root = match &*target {
        TargetLocator::LocalBare { worker_root } => worker_root.clone(),
        other => panic!("expected the retained local target, got {other:?}"),
    };
    let workspace_marker = worker_root
        .join("workspace")
        .join("untracked-before-restart");
    let temporary_marker = worker_root.join("tmp").join("temporary-before-restart");
    std::fs::create_dir_all(workspace_marker.parent().unwrap()).unwrap();
    std::fs::create_dir_all(temporary_marker.parent().unwrap()).unwrap();
    std::fs::write(&workspace_marker, b"workspace survives").unwrap();
    std::fs::write(&temporary_marker, b"temporary data survives").unwrap();
    crate::database::save_session(record).unwrap();

    // Mail the worker accepted and never handed to the agent; the daemon has
    // marked it accepted and will not send it again (#1616). Submit it through
    // the live worker before retiring it so only one process ever writes to
    // `relay_root` (a retired actor reaps its stand-in relay asynchronously).
    let waiting = mj_core::mailbox::MailboxEvent {
        key: "waiting-message".into(),
        source: "parent".into(),
        wake: false,
        created_at_ms: 1,
        body: mj_core::mailbox::MailboxEventBody::PlainText {
            text: "read this with your next prompt".into(),
        },
    };
    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    let mut previous_lease = previous_worker.lease_connection(&admission).await.unwrap();
    previous_lease
        .connection_mut()
        .submit(
            "deliver-waiting".into(),
            RelayCommand::DeliverMailboxEvent {
                event: waiting.clone(),
            },
        )
        .await
        .unwrap();
    drop(previous_lease);

    // The prior worker and its session actor are gone. The test publishes a
    // checkpoint-only relay process only once Restart has durably entered its
    // fresh-checkpoint phase, just as target recovery reconnects the journal.
    channels.targets.send_replace(Vec::new());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !previous_worker.is_stopped() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the dead worker actor did not retire");
    let mut restored_target =
        latch_relay_target(&relay_root, Some(&starts), ReleaseSupport::Supported, false);
    // A current worker: it exports its notification stack and its mail.
    restored_target.spec.env.insert(
        LATCH_RELAY_MAX_PROTOCOL.into(),
        mj_core::relay::RELAY_PROTOCOL_VERSION.to_string(),
    );
    let targets = channels.targets.clone();
    let restore_actor = mj_core::runtime::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            loop {
                let state = crate::database::load_session_record(LATCH_RELAY_SESSION)
                    .ok()
                    .flatten()
                    .map(|record| record.state);
                if state == Some(SessionState::Closing) {
                    targets.send_replace(vec![restored_target]);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Restart never persisted its closing checkpoint intent");
    });
    let executor = RestartCheckpointExecutor {
        archive_directory,
        ..Default::default()
    };
    controller
        .suspend_session_for_restart(
            &admission,
            LATCH_RELAY_SESSION,
            &executor,
            &channels.control,
            None,
            false,
        )
        .await
        .unwrap();
    restore_actor.await.unwrap();
    channels.shutdown.shutdown().await.unwrap();

    let sealed = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(sealed.state, SessionState::Closing);
    assert_eq!(sealed.target, Some(target));
    assert_eq!(sealed.last_error, None);
    assert!(sealed.last_checkpoint_error.is_none());
    assert_eq!(sealed.checkpoint.as_deref(), Some(old_checkpoint.as_ref()));
    let intent = crate::database::load_session_restart(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    let handoff = intent.handoff.as_ref().expect("fresh restart checkpoint");
    let archived = verify_archive_streaming(&handoff.archive_path).unwrap();
    let stack = archived
        .canonical_session
        .notification_stack
        .expect("a stack-capable worker exports an explicit stack");
    assert!(stack.entries.is_empty());
    assert_eq!(
        archived
            .canonical_session
            .mailbox
            .expect("the restart archive kept the worker's mail")
            .pending_events,
        vec![waiting]
    );
    assert_eq!(
        intent.checkpoint_sha256.as_ref(),
        intent.handoff.as_ref().map(|handoff| &handoff.sha256)
    );
    assert_eq!(
        std::fs::read(workspace_marker).unwrap(),
        b"workspace survives"
    );
    assert_eq!(
        std::fs::read(temporary_marker).unwrap(),
        b"temporary data survives"
    );
    assert_eq!(std::fs::read_to_string(starts).unwrap().lines().count(), 1);

    let purposes = executor.purposes();
    assert!(
        purposes
            .iter()
            .any(|purpose| purpose == "export target checkpoint"),
        "Restart must capture a new target archive despite the installed checkpoint: {purposes:?}"
    );
    assert!(
        purposes.iter().all(|purpose| {
            let purpose = purpose.to_lowercase();
            !purpose.contains("remove exact")
                && !purpose.contains("remove container")
                && !purpose.contains("temporary volume")
                && !purpose.contains("cleanup stopped target")
        }),
        "Restart must seal the retained target without target cleanup: {purposes:?}"
    );
}

/// A worker that answers a Close with a refusal did not seal its relay: the
/// session is still live, and nothing about the refused close can be resumed.
/// The suspend therefore releases its barrier and returns the session to
/// `Running` with the refusal as its visible error, the way a close that stops
/// before sealing does. Recording an interrupted close instead left I2-3's
/// session `suspending` with an error until someone suspended it again.
///
/// The refusal comes from a question the harness asks between the cut and the
/// Close: a question cannot wait behind the cut, so it spoils it. This goes
/// through the daemon's route: its suspend marks the record `Closing` first
/// and then recovers that close.
// Hard-won: d45082024979: a worker refusal left a suspended session stuck in Closing
#[cfg(unix)]
#[tokio::test]
async fn a_refused_close_returns_the_session_to_running_and_releases_its_barrier() {
    if !in_close_cut_child(
        "a_refused_close_returns_the_session_to_running_and_releases_its_barrier",
        LATE_HARNESS_QUESTION,
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (mut controller, channels, handle) = close_cut_controller(SessionState::Closing).await;
    let suspended = controller
        .recover_interrupted_close_managed(
            &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
            LATCH_RELAY_SESSION,
            &SucceedingExecutor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
            crate::controller::lifecycle::SuspendSettlement::Suspend,
        )
        .await;

    let error = suspended.expect_err("the scripted worker refuses the Close");
    assert!(
        format!("{error:#}").contains("close does not match the current checkpoint cut"),
        "{error:#}"
    );
    let record = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(
        record.state,
        SessionState::Running,
        "{:?}",
        record.last_error
    );
    assert!(
        record
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("close does not match the current checkpoint cut")),
        "{:?}",
        record.last_error
    );
    let persisted = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(persisted.state, SessionState::Running);

    // The barrier is gone and the relay was never sealed, so the session can
    // take work again.
    wait_until_the_actor_serves_again(&handle).await;
    let mut lease = handle
        .lease_connection(&crate::controller::test_support::session_admission(
            handle.session_id(),
        ))
        .await
        .unwrap();
    let snapshot = lease.connection_mut().sync().await.unwrap();
    lease.release();
    assert_eq!(snapshot.operational.checkpoint_barrier, None);
    assert!(
        !matches!(
            snapshot.operational.execution,
            RelayExecutionState::Closing | RelayExecutionState::Closed
        ),
        "{:?}",
        snapshot.operational.execution
    );
    channels.shutdown.shutdown().await.unwrap();
}
/// A direct `mj checkpoint` runs the controller's checkpoint futures inside
/// the daemon's connection task, on a 2 MiB Tokio worker stack. In a debug
/// build the capture protocol's own future is very large, and every wrapper
/// that embeds it inline carries a full copy on that stack, so the daemon
/// overflowed it (`mj checkpoint --session` aborted the daemon). The capture
/// future must stay boxed below the wrappers, so the ones above it stay small.
// Hard-won: 1e384802787f: direct checkpoint overflowed the daemon worker thread stack
#[test]
fn the_checkpoint_entry_futures_do_not_embed_the_capture_future() {
    // Measured in a debug build: 50000 and 39360 bytes unboxed, 24040 and 72
    // boxed. The wrapper now also carries its relay lease inline (about 4 KiB
    // with the session's operational state), so its limit only needs to
    // separate that from an unboxed capture future.
    const WRAPPER_LIMIT_BYTES: usize = 8 * 1024;
    let controller = Controller {
        config: Default::default(),
        state: State::default(),
    };
    let admission = crate::controller::test_support::session_admission("session-1");
    let executor = RefusingExecutor("not used");
    let consent = mj_core::suspend_consent::CaptureConsent::default();
    let latched = controller.checkpoint_session_latched_with_recovery_stage(
        &admission,
        "session-1",
        &executor,
        None,
        LatchExclusivity::ReleaseAfterLatch,
        CheckpointExportPolicy::Always,
        CheckpointDelivery::DownloadToDaemon,
        false,
        None,
        None,
        &consent,
    );
    let latched_size = std::mem::size_of_val(&latched);
    drop(latched);
    assert!(
        latched_size < WRAPPER_LIMIT_BYTES,
        "the latched wrapper future is {latched_size} bytes (limit {WRAPPER_LIMIT_BYTES}); box the \
         capture future so the wrappers stay small"
    );
}

#[cfg(unix)]
const RELAY_JOB_TEST_CHILD: &str = "MJ_TEST_RELAY_JOB_CHILD";

/// Credential sync used to open a relay connection of its own to every live
/// worker every minute. It now runs as a job on the session actor's own
/// connection: no second connection reaches the worker, and while a
/// lifecycle operation holds the connection the job is deferred rather than
/// connecting around it.
// Hard-won: a6ec3266c340: periodic credential sync opened about nine throwaway remote relay connections per session every five minutes
#[cfg(unix)]
#[tokio::test]
async fn a_relay_job_runs_on_the_actors_own_connection_and_defers_to_a_lease() {
    if std::env::var_os(RELAY_JOB_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::a_relay_job_runs_on_the_actors_own_connection_and_defers_to_a_lease",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(RELAY_JOB_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();

    struct StatusJob(tokio::sync::oneshot::Sender<Result<()>>);
    impl crate::session_manager::RelayConnectionJob for StatusJob {
        fn run<'a>(
            self: Box<Self>,
            client: &'a mut crate::worker_client::RelayClient,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                let _ = self.0.send(client.status().await.map(|_| ()));
            })
        }
        fn refuse(self: Box<Self>, error: anyhow::Error) {
            let _ = self.0.send(Err(error));
        }
    }
    async fn run_status(handle: &ManagedSessionHandle) -> Result<()> {
        let (reply, response) = tokio::sync::oneshot::channel();
        handle.run_on_connection(Box::new(StatusJob(reply))).await;
        response.await.unwrap()
    }

    let relay_root = tempfile::tempdir().unwrap();
    let start_log = tempfile::tempdir().unwrap();
    let start_log = start_log.path().join("relay-starts");
    crate::database::save_session(&checkpoint_test_session(LATCH_RELAY_SESSION)).unwrap();
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            relay_root.path(),
            Some(&start_log),
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    let handle = channels
        .control
        .wait_for_session(LATCH_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    wait_until_the_actor_serves_again(&handle).await;
    assert_eq!(relay_starts(&start_log), 1);

    run_status(&handle).await.unwrap();
    assert_eq!(
        relay_starts(&start_log),
        1,
        "the job must use the actor's connection, not open another"
    );

    let lease = handle
        .lease_connection(&crate::controller::test_support::session_admission(
            handle.session_id(),
        ))
        .await
        .unwrap();
    let deferred = run_status(&handle).await.unwrap_err();
    assert!(
        crate::session_manager::RelayJobDeferred::marks(&deferred),
        "{deferred:#}"
    );
    lease.release();
    wait_until_the_actor_serves_again(&handle).await;
    run_status(&handle).await.unwrap();
    assert_eq!(relay_starts(&start_log), 1);
    channels.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1331: a preceding report warning concealed the actual capture failure.
#[test]
fn checkpoint_failure_summary_names_the_error_after_report_warnings() {
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        workspace_storage: Default::default(),
    };
    for (stderr, expected) in [
        (
            "sub-agent reports exceed the 67108864-byte checkpoint limit; the oldest 3 were left out\nError: repository capture failed\n\nCaused by:\n    unsupported untracked file type at app/.mj/lanes\n",
            "repository capture failed: unsupported untracked file type at app/.mj/lanes",
        ),
        (
            "sub-agent reports exceed the 67108864-byte checkpoint limit; the oldest 3 were left out\n",
            "target exited without reporting its error",
        ),
    ] {
        let executor = ExportExecutor::new(1, stderr);
        let error = run_checkpoint_staging_command(
            &executor,
            &locator,
            LATCH_RELAY_SESSION,
            &export_spec_fixture(),
            export_stdin_command,
            "capture target checkpoint",
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("capture target checkpoint failed with status 1: {expected}")
        );
    }
}

// Hard-won: #1331: successful captures silently omitted reports, and retries must not flood the transcript.
#[cfg(unix)]
#[tokio::test]
async fn checkpoint_report_notices_are_deduplicated_in_the_relay_transcript() {
    const CHILD: &str = "MJ_TEST_CHECKPOINT_REPORT_NOTICE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::checkpoint_report_notices_are_deduplicated_in_the_relay_transcript",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let relay_root = tempfile::tempdir().unwrap();
    let (_channels, _handle, mut relay, barrier_command_id, _) =
        latch_a_live_checkpoint(relay_root.path(), None, ReleaseSupport::Supported, false).await;
    relay
        .submit(
            new_command_id("end-report-test-barrier").unwrap(),
            RelayCommand::ReleaseCheckpoint { barrier_command_id },
        )
        .await
        .unwrap();
    let mut captured = CapturedCheckpoint {
        stage_path: "stage".into(),
        native_bytes: 0,
        repository_bytes: 0,
        reused_native: false,
        omitted_reports: mj_checkpoint::checkpoint::CheckpointReportOmissions {
            files: 3,
            bytes: 128 * 1024 * 1024,
            largest_directories: vec![mj_checkpoint::checkpoint::CheckpointReportDirectory {
                child_id: "large-child".into(),
                bytes: 150 * 1024 * 1024,
            }],
        },
    };
    for _ in 0..2 {
        submit_checkpoint_report_notice(&mut relay, &captured)
            .await
            .unwrap();
    }
    let notices = |snapshot: &mj_core::state::ManagedSessionSnapshot| {
        snapshot
            .materialized
            .transcript
            .iter()
            .filter_map(|item| {
                if let mj_core::transcript::TranscriptBody::System { text } = &item.body {
                    text.starts_with("This checkpoint left out ")
                        .then(|| text.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let first = notices(&relay.sync_snapshot().await.unwrap());
    assert_eq!(first.len(), 1);
    assert!(first[0].contains("3 sub-agent report files (128 MiB)"));
    assert!(first[0].contains("large-child (150 MiB)"));
    // Reconstructing the caller from capture JSON does not lose de-duplication.
    captured = serde_json::from_slice(&serde_json::to_vec(&captured).unwrap()).unwrap();
    submit_checkpoint_report_notice(&mut relay, &captured)
        .await
        .unwrap();
    assert_eq!(notices(&relay.sync_snapshot().await.unwrap()), first);
    captured.omitted_reports = Default::default();
    submit_checkpoint_report_notice(&mut relay, &captured)
        .await
        .unwrap();
    submit_checkpoint_report_notice(&mut relay, &captured)
        .await
        .unwrap();
    assert_eq!(notices(&relay.sync_snapshot().await.unwrap()).len(), 2);
    captured.omitted_reports.files = 3;
    submit_checkpoint_report_notice(&mut relay, &captured)
        .await
        .unwrap();
    assert_eq!(notices(&relay.sync_snapshot().await.unwrap()).len(), 3);
}

#[cfg(unix)]
#[tokio::test]
async fn suspended_archive_serializes_compact_tool_and_terminal_bodies() {
    use mj_core::relay::RelayObservation;
    use mj_core::transcript::{TerminalOutputRecord, TranscriptBody};

    if !in_close_cut_child(
        "suspended_archive_serializes_compact_tool_and_terminal_bodies",
        LATE_HARNESS_NOTIFICATION,
    ) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let data_dir = mj_core::config::data_dir();
    let relay_root = data_dir.join("relay");
    std::fs::create_dir_all(&relay_root).unwrap();
    let mut relay =
        mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    for observation in [
        RelayObservation::SessionOpened {
            native_session_id: "native-session".into(),
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
            resumed: true,
        },
        RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        },
    ] {
        relay.record_observation(observation).unwrap();
    }
    let mut full_body_bytes = 0;
    for turn in 1..=2 {
        relay
            .record_observation(RelayObservation::HarnessTurnStarted {
                started_at_ms: turn,
            })
            .unwrap();
        let output = "tool output\n".repeat(8 * 1024);
        let patch = (0..8192)
            .map(|line| format!("changed line {line:04}\n"))
            .collect::<String>();
        let raw_input = "i".repeat(64 * 1024);
        let tool = serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": format!("edit-{turn}"),
            "title": "Edit", "kind": "edit", "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": output}},
                {"type": "diff", "path": "notes.txt", "newText": patch},
            ],
            "rawInput": raw_input,
        });
        full_body_bytes += serde_json::to_vec(&tool).unwrap().len();
        relay
            .record_session_update(serde_json::from_value(tool).unwrap())
            .unwrap();
        let terminal_id = format!("unclaimed-{turn}");
        let output = "one\n".repeat(64 * 1024);
        let terminal = TranscriptBody::terminal_output(TerminalOutputRecord {
            terminal_id: terminal_id.clone(),
            output: output.clone().into(),
            truncated: false,
            exit_code: Some(1),
            signal: None,
        });
        full_body_bytes += serde_json::to_vec(&terminal).unwrap().len();
        relay
            .record_observation(RelayObservation::TerminalOutput {
                terminal_id,
                output,
                truncated: false,
                exit_code: Some(1),
                signal: None,
            })
            .unwrap();
        relay
            .record_observation(RelayObservation::HarnessTurnSettled {
                autonomous: None,
                origin: None,
                prompt_in_flight: false,
            })
            .unwrap();
    }
    drop(relay);

    let (mut controller, channels, handle) = close_cut_controller(SessionState::Running).await;
    wait_until_the_actor_serves_again(&handle).await;
    let archive_directory = data_dir.join("fresh-checkpoints");
    std::fs::create_dir_all(&archive_directory).unwrap();
    let executor = RestartCheckpointExecutor {
        archive_directory,
        ..Default::default()
    };
    let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
    let result = controller
        .suspend_session_managed_controlled(
            &admission,
            LATCH_RELAY_SESSION,
            &executor,
            &channels.control,
            &mj_core::suspend_consent::SuspendConsent::default(),
            None,
            crate::controller::lifecycle::SuspendSettlement::Suspend,
        )
        .await;
    channels.shutdown.shutdown().await.unwrap();
    result.unwrap();

    let record = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(record.state, SessionState::Stopped);
    let checkpoint = record.checkpoint.as_ref().unwrap();
    let verified = verify_archive_streaming(&checkpoint.archive_path).unwrap();
    let restored = mj_transcript::projection::materialized_session_from_canonical(
        LATCH_RELAY_SESSION,
        &verified.canonical_session,
    )
    .unwrap();
    assert_eq!(
        restored
            .transcript
            .iter()
            .filter(|item| item.body.detail_trimmed())
            .count(),
        4
    );
    let mut archive =
        zip::ZipArchive::new(std::fs::File::open(&checkpoint.archive_path).unwrap()).unwrap();
    let canonical_bytes = archive.by_name("canonical/session.json").unwrap().size();
    assert!(
        canonical_bytes * 5 < full_body_bytes as u64,
        "{canonical_bytes} of {full_body_bytes}"
    );
    let database = rusqlite::Connection::open(crate::database::database_path()).unwrap();
    let body_bytes: u64 = database.query_row(
        "SELECT sum(length(CAST(body_json AS BLOB))) FROM materialized_transcript_items WHERE session_id = ?1",
        [LATCH_RELAY_SESSION], |row| row.get(0),
    ).unwrap();
    assert!(
        body_bytes * 5 < full_body_bytes as u64,
        "{body_bytes} of {full_body_bytes}"
    );

    let details = data_dir
        .join("tool-details")
        .join(format!("{LATCH_RELAY_SESSION}.mjtd"));
    assert!(details.exists());
    crate::database::delete_session(LATCH_RELAY_SESSION).unwrap();
    assert!(!details.exists());
}

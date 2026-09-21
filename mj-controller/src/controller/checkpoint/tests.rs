use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
#[cfg(unix)]
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use anyhow::Result;

#[cfg(unix)]
use crate::controller::now;
use crate::controller::restore_session_after_persistence_failure;
#[cfg(unix)]
use crate::controller::test_support::IsolatedTest;
use crate::controller::test_support::{
    RefusingExecutor, checkpoint_test_session, write_checkpoint_gate_archive,
};
#[cfg(unix)]
use crate::session_manager::{ManagedSessionHandle, new_command_id};
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
use mj_transcript::projection::canonical_session_from_materialized;

#[cfg(unix)]
use crate::targets::ProvisionStage;
use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec};
#[cfg(unix)]
use mj_core::relay::RelayCommandOutcome;
use mj_core::relay::{RelayCommand, RelayCursor, RelayExecutionState};

use super::*;

/// An executor that fails if it is used. The layout of a session whose
/// repositories are described by configuration is derived without touching
/// the target at all.
#[test]
fn the_export_layout_places_each_session_kind_in_its_workspace() {
    let session_id = "1123456789abcdef0123456789abcdef";
    let mut config = crate::controller::test_support::resume_compatibility_config();
    config.bundles.insert(
        "app-bundle".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "app".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "app".into(),
                github: None,
                local: None,
                destination: PathBuf::from("app"),
                git_ref: None,
            }],
        },
    );

    // A bundle session's repositories are laid out under the target's own
    // workspace directory.
    let mut session = checkpoint_test_session(session_id);
    session.bundle_id = "app-bundle".into();
    session.target = Some(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "hel-session".into(),
        workspace_storage: Default::default(),
    });
    let mut state = State::default();
    state.sessions.insert(session_id.into(), session.clone());
    let controller = Controller {
        config: config.clone(),
        state,
    };

    let layout = controller
        .session_export_layout(session_id, &RefusingExecutor("the export layout"))
        .unwrap();
    assert_eq!(layout.workspace_root, "/workspace");
    assert_eq!(layout.primary_repository, "app");
    assert_eq!(
        layout
            .repositories
            .iter()
            .map(|repository| (
                repository.id.clone(),
                repository.relative_destination.clone()
            ))
            .collect::<Vec<_>>(),
        [("app".to_owned(), PathBuf::from("app"))]
    );
    assert!(matches!(
        layout.repositories[0].capture,
        CheckpointRepositoryCapture::RemoteWorkspace
    ));
    assert!(layout.managed_worktree.is_none());

    // A bare checkout is its own workspace: the directory's parent, plus
    // the checkout itself as the one repository.
    let mut raw = session;
    raw.target_template_id = "local-bare".into();
    raw.project_directory = Some(PathBuf::from("/home/dev/project"));
    raw.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/home/dev/.local/share/hel/workers/session"),
    });
    let mut state = State::default();
    state.sessions.insert(session_id.into(), raw);
    let controller = Controller { config, state };

    let layout = controller
        .session_export_layout(session_id, &RefusingExecutor("the export layout"))
        .unwrap();
    assert_eq!(layout.workspace_root, "/home/dev");
    assert_eq!(layout.primary_repository, "project");
    assert_eq!(
        layout.repositories[0].relative_destination,
        PathBuf::from("project")
    );
    assert!(matches!(
        layout.repositories[0].capture,
        CheckpointRepositoryCapture::MetadataOnly
    ));
}

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

    session.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/home/dev/.local/share/hel/workers/session"),
    });
    let mut state = State::default();
    state.sessions.insert(session_id.into(), session);
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

#[test]
fn a_managed_worktree_checkpoint_bundles_from_the_recorded_base() {
    let (capture, creation_commit, worktree_head) = managed_worktree_export_capture(false);
    let CheckpointRepositoryCapture::DeltaFrom { base_commit } = capture else {
        panic!("a managed worktree must be captured as a delta, got {capture:?}");
    };
    assert_eq!(base_commit, creation_commit);
    assert_ne!(base_commit, worktree_head);
}

#[test]
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
    let mut state = State::default();
    let mut session = checkpoint_test_session(session_id);
    session.checkpoint = Some(CheckpointMetadata {
        archive_path: directory.path().join(&referenced_name),
        sha256: "c".repeat(64),
        created_at: "2026-08-12T00:00:00Z".into(),
        event_frontier: 7,
    });
    state.sessions.insert(session_id.into(), session);

    assert_eq!(
        reconcile_managed_checkpoint_archives_in(directory.path(), &state).unwrap(),
        1
    );
    assert!(directory.path().join(referenced_name).exists());
    assert!(!directory.path().join(orphan_name).exists());
    assert!(directory.path().join(imported_name).exists());
    assert!(directory.path().join("notes.hel.zip").exists());
}
#[test]
fn recovery_artifact_final_verification_checks_the_archive_digest() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "1123456789abcdef0123456789abcdef";
    let metadata = write_checkpoint_gate_archive(directory.path(), session_id, 7);
    let mut artifact = CheckpointArtifact {
        metadata,
        native_session_id: "native-session".into(),
        event_frontier_digest: "a".repeat(64),
    };

    verify_checkpoint_artifact(session_id, &artifact).unwrap();
    artifact.metadata.sha256 = "b".repeat(64);
    assert!(
        verify_checkpoint_artifact(session_id, &artifact)
            .unwrap_err()
            .to_string()
            .contains("checkpoint SHA changed")
    );
}
/// A snapshot of a session whose checkpoint barrier is open but not yet
/// ready, projected exactly at `cursor`.
fn checkpoint_barrier_snapshot(cursor: &RelayCursor) -> ManagedSessionSnapshot {
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = cursor.ordinal;
    materialized.applied_event_digest = cursor.digest.clone();
    ManagedSessionSnapshot {
        subagent_requests: Vec::new(),
        subagent_results: Vec::new(),
        window: mj_core::state::ProjectionWindow::of(&materialized),
        materialized,
        latest_credential_sync_signal: None,
        worker_build: None,
        operational: mj_core::relay::RelayOperationalState {
            jev_decision_id: None,
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
            goal: serde_json::from_value(
                serde_json::json!({"known":true,"execution":{"version":1,"status":"idle"}}),
            )
            .unwrap(),
            capacity_retry: None,
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
            agent_capabilities: None,
            agent_info: None,
            steering_supported: None,
            config_options: Vec::new(),
            modes: None,
            available_commands: Vec::new(),
            config: BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: Some("checkpoint-1".into()),
            checkpoint_ready: None,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            tools_in_flight: Vec::new(),
            activity: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
            background_work_known: None,
        },
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
    })
    .unwrap()
}
fn export_spec_fixture() -> CheckpointExportSpec {
    CheckpointExportSpec {
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
        canonical_session: canonical_session_from_materialized(&MaterializedSession::empty(
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
#[test]
fn docker_checkpoint_fallback_upload_uses_docker_cp() {
    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }
    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    let executor = RecordingExecutor {
        commands: RefCell::new(Vec::new()),
    };
    let locator = targets::TargetLocator::LocalDocker {
        borrowed_from: None,
        container_id: "hel-session-12345678".to_owned(),
    };
    upload_checkpoint_spec(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        Path::new("checkpoint-spec.json"),
        "/var/lib/hel/workers/session/checkpoint-spec.json",
    )
    .unwrap();

    let commands = executor.commands.borrow();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, "docker");
    assert_eq!(
        commands[0].args,
        [
            "cp",
            "checkpoint-spec.json",
            "hel-session-12345678:/var/lib/hel/workers/session/checkpoint-spec.json"
        ]
    );
    assert_eq!(commands[0].purpose, "upload checkpoint specification");
}
#[test]
fn checkpoint_export_streams_its_spec_instead_of_uploading_it() {
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        workspace_storage: Default::default(),
    };
    let spec = export_spec_fixture();
    let executor = ExportExecutor::new(0, "");

    let output = run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &spec,
        export_stdin_command,
        "export target checkpoint",
        None,
    )
    .unwrap();

    assert_eq!(output.stdout, exported_checkpoint_json());
    assert_eq!(
        serde_json::from_slice::<CheckpointExportSpec>(&executor.streamed_spec.borrow()).unwrap(),
        spec
    );
    assert_eq!(
        executor.purposes.into_inner(),
        vec!["export target checkpoint".to_owned()]
    );
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
/// The explicit export protocol field makes every older worker reject the
/// current spec before it can apply obsolete path or collection behavior.
#[test]
fn a_legacy_export_worker_is_replaced_before_it_runs_obsolete_behavior() {
    let locator = targets::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: targets::resource_name(LATCH_RELAY_SESSION).unwrap(),
        workspace_storage: Default::default(),
    };
    let spec = export_spec_fixture();
    let executor = ExportExecutor::new(
        1,
        "Error: parse checkpoint export spec from standard input\n\nCaused by:\n    \
             unknown field `protocol_version`, expected `session` at line 1 column 20\n",
    )
    .retry_stdin_after_failure();
    let worker_binary = Path::new("/hel-test-worker");

    let output = run_checkpoint_staging_command(
        &executor,
        &locator,
        LATCH_RELAY_SESSION,
        &spec,
        export_stdin_command,
        "export target checkpoint",
        Some(worker_binary),
    )
    .unwrap();

    assert_eq!(output.stdout, exported_checkpoint_json());
    assert_eq!(
        serde_json::from_slice::<CheckpointExportSpec>(&executor.streamed_spec.borrow()).unwrap(),
        spec
    );
    assert_eq!(
        executor.purposes.into_inner(),
        vec![
            "export target checkpoint".to_owned(),
            "stage replacement Mjolnir worker".to_owned(),
            "assign replacement worker to the worker user".to_owned(),
            "replace installed Mjolnir worker".to_owned(),
            "make replaced Mjolnir worker executable".to_owned(),
            "export target checkpoint".to_owned(),
        ]
    );
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
            false,
            false,
        )
        .is_none()
    );
    let interrupted = checkpoint_barrier_wait_ended(
        &snapshot,
        "checkpoint-1",
        BarrierBusyPolicy::InterruptWhileRunning,
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
        status: agent_client_protocol::schema::v1::ToolCallStatus::InProgress,
        started_at_ms: 1,
    }];
    let tool_deferred = checkpoint_barrier_wait_ended(
        &snapshot,
        "checkpoint-1",
        BarrierBusyPolicy::DeferWhileRunning,
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

#[test]
fn a_stuck_checkpoint_barrier_is_retried_by_restarting_the_worker() {
    // Both ways the wait can end without a barrier, each wrapped the way
    // the checkpoint path wraps them, and each still asking for the retry.
    for failure in [
        CheckpointBarrierUnreachable::not_admitted("checkpoint-976f6746887c5ccd93b9d8bbe120ef06"),
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
#[test]
fn a_dead_worker_hello_failure_is_retried_by_restarting_the_worker() {
    let dead = anyhow::Error::new(RelayTransportDead::new("the proxy is gone"))
        .context("connect to the session worker for checkpoint");
    assert!(worker_connect_needs_restart(&dead), "{dead:#}");
    assert!(!worker_connect_needs_restart(&anyhow::anyhow!(
        "unknown session"
    )));
}
#[cfg(unix)]
#[tokio::test]
async fn checkpoint_restart_stop_failure_names_mjolnir() {
    struct FailingStop;

    impl CommandExecutor for FailingStop {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: b"permission denied".to_vec(),
            })
        }
    }

    let session_id = "0123456789abcdef0123456789abcdef";
    let worker_root = format!("/tmp/mjolnir-checkpoint-test/{session_id}");
    let backend = targets::TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    };
    let controller = Controller {
        config: Config::default(),
        state: State::default(),
    };
    let reconnect = CommandSpec::new("unused", std::iter::empty::<&str>());

    let result = controller
        .restart_worker_for_checkpoint(session_id, &FailingStop, &backend, &worker_root, &reconnect)
        .await;
    let error = match result {
        Ok(_) => panic!("a failed worker stop unexpectedly restarted the checkpoint worker"),
        Err(error) => error,
    };
    let detail = format!("{error:#}");
    assert!(
        detail.starts_with("stop wedged Mjolnir worker before retrying checkpoint"),
        "{detail}"
    );
    assert!(detail.contains("permission denied"), "{detail}");
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
pub(crate) const LATCH_RELAY_ROOT: &str = "MJ_TEST_LATCH_RELAY_ROOT";
const LATCH_RELAY_STARTS: &str = "MJ_TEST_LATCH_RELAY_STARTS";
const LATCH_RELAY_REJECT_RELEASE: &str = "MJ_TEST_LATCH_REJECT_RELEASE";
#[cfg(unix)]
const LATCH_RELAY_RUNNING: &str = "MJ_TEST_LATCH_RELAY_RUNNING";
#[cfg(unix)]
const LATCH_TEST_CHILD: &str = "MJ_TEST_LATCH_CHILD";
#[cfg(unix)]
const ABANDON_TEST_CHILD: &str = "MJ_TEST_ABANDON_LATCH_CHILD";
#[cfg(unix)]
const RELEASE_TEST_CHILD: &str = "MJ_TEST_RELEASE_LATCH_CHILD";
#[cfg(unix)]
const LEGACY_RELEASE_TEST_CHILD: &str = "MJ_TEST_LEGACY_RELEASE_LATCH_CHILD";
#[cfg(unix)]
const REUSE_TEST_CHILD: &str = "MJ_TEST_REUSE_LATCH_CHILD";
pub(crate) const LATCH_CHECKPOINT_ONLY: &str = "MJ_TEST_LATCH_CHECKPOINT_ONLY";
const LATCH_RELAY_STARTUP_DELAY_MS: &str = "MJ_TEST_LATCH_STARTUP_DELAY_MS";
pub(crate) const LATCH_RELAY_SESSION: &str = "018f9dd2-a3b4-7c8d-9000-0123456789ab";
/// Whether the scripted relay understands the early checkpoint release.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseSupport {
    Supported,
    /// Answer a release exactly as a worker that predates the command does:
    /// its `RelayCommand` cannot deserialize the variant at all.
    Rejected,
}
/// Relay server half of the checkpoint latch test.
///
/// A durable relay only reports a checkpoint barrier ready once a dispatch
/// driver claims it, so this also runs the one step the worker runtime
/// performs for a barrier. It does nothing unless a parent test points it
/// at a relay journal root.
#[test]
fn latch_relay_child_serves_stdio() {
    let Some(root) = std::env::var_os(LATCH_RELAY_ROOT) else {
        return;
    };
    // With `--nocapture` libtest writes `test <name> ... ` without a
    // trailing newline before the body runs. End that line first so it
    // cannot glue itself onto the first protocol frame.
    println!();
    // Record this start so a parent test can tell a reconnect from a reused
    // connection.
    if let Some(starts) = std::env::var_os(LATCH_RELAY_STARTS) {
        use std::io::Write;
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(starts)
            .expect("open the relay start log");
        writeln!(log, "{}", std::process::id()).expect("record this relay start");
    }
    let checkpoint_only = std::env::var_os(LATCH_CHECKPOINT_ONLY).is_some();
    let mut relay = if checkpoint_only {
        mj_worker::relay::DurableRelay::open_for_checkpoint(
            Path::new(&root),
            LATCH_RELAY_SESSION,
            "1.0.0",
        )
    } else {
        mj_worker::relay::DurableRelay::open(Path::new(&root), LATCH_RELAY_SESSION, "1.0.0")
    }
    .expect("open the test relay journal");
    if relay.operational_state().native_session_id.is_none() {
        relay
            .record_observation(mj_core::relay::RelayObservation::SessionOpened {
                native_session_id: "native-session".into(),
                native_continuity_lost: false,
                resumed: true,
            })
            .unwrap();
    }
    if !relay.operational_state().goal.synchronized() && !checkpoint_only {
        relay.record_session_update(serde_json::from_value(serde_json::json!({
        "sessionUpdate":"session_info_update", "_meta":{"goal":null,"execution":{"version":1,"status":"idle"}}
    })).unwrap()).unwrap();
    }
    let ready_at = Instant::now()
        + Duration::from_millis(
            std::env::var(LATCH_RELAY_STARTUP_DELAY_MS)
                .ok()
                .map(|value| value.parse::<u64>().unwrap())
                .unwrap_or(0),
        );
    let reject_release = std::env::var_os(LATCH_RELAY_REJECT_RELEASE).is_some();
    #[cfg(unix)]
    let running = std::env::var_os(LATCH_RELAY_RUNNING).is_some();
    #[cfg(unix)]
    if running && relay.operational_state().active_prompt.is_none() {
        let response = relay.handle(mj_core::relay::RelayRequestEnvelope {
            request_id: "seed-running-request".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: mj_core::relay::RelayRequest::Submit {
                command_id: "seed-running-prompt".into(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("running"))],
                },
            },
        });
        assert!(matches!(
            response.body,
            mj_core::relay::RelayResponseBody::Ok {
                payload: mj_core::relay::RelayResponsePayload::Accepted { .. }
            }
        ));
        let claimed = relay
            .claim_pending_commands(true)
            .expect("seed the running prompt");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].command_id, "seed-running-prompt");
    }
    let mut reader = std::io::stdin().lock();
    let mut writer = std::io::stdout().lock();
    let mut configured = false;
    while let Some(request) =
        mj_core::relay::read_relay_frame(&mut reader).expect("read a relay request")
    {
        if !checkpoint_only && !configured && Instant::now() >= ready_at {
            relay
                .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
                    config_options: Vec::new(),
                })
                .unwrap();
            configured = true;
        }
        if matches!(
            &request.request,
            mj_core::relay::RelayRequest::Submit {
                command: RelayCommand::BeginCheckpoint { .. },
                ..
            }
        ) {
            assert!(
                checkpoint_only || relay.operational_state().native_session_is_ready(),
                "checkpoint submitted before current ACP startup finished"
            );
        }
        let response = if reject_release && requests_checkpoint_release(&request) {
            unparseable_request_response(&request)
        } else {
            relay.handle(request)
        };
        mj_core::relay::write_relay_frame(&mut writer, &response).expect("answer a relay request");
        if checkpoint_only {
            relay.dispatch_checkpoint_only().unwrap();
        }
        for claimed in relay
            .claim_pending_commands(true)
            .expect("claim relay commands")
        {
            match claimed.command {
                RelayCommand::BeginCheckpoint { .. } => {
                    relay
                        .record_checkpoint_ready(&claimed.command_id)
                        .expect("report the checkpoint barrier ready");
                }
                #[cfg(unix)]
                RelayCommand::CancelTurn => {
                    let prompt_id = relay
                        .operational_state()
                        .active_prompt
                        .as_ref()
                        .map(|prompt| prompt.command_id.clone())
                        .expect("a prompt to cancel");
                    relay
                        .record_command_completed(
                            &claimed.command_id,
                            RelayCommandOutcome::Cancelled,
                        )
                        .expect("complete the cancellation");
                    relay
                        .record_command_completed(
                            &prompt_id,
                            RelayCommandOutcome::Prompt {
                                diagnostic: None,
                                stop_reason: "cancelled".into(),
                                usage: None,
                            },
                        )
                        .expect("complete the cancelled prompt");
                }
                _ => {}
            }
        }
    }
}
fn requests_checkpoint_release(request: &mj_core::relay::RelayRequestEnvelope) -> bool {
    matches!(
        &request.request,
        mj_core::relay::RelayRequest::Submit {
            command: RelayCommand::ReleaseCheckpoint { .. },
            ..
        }
    )
}
/// The answer a worker gives for a frame its own protocol cannot decode.
/// An older `RelayCommand` has no `release_checkpoint` variant, and the
/// enum denies unknown ones, so the request never reaches its relay.
fn unparseable_request_response(
    request: &mj_core::relay::RelayRequestEnvelope,
) -> mj_core::relay::RelayResponseEnvelope {
    mj_core::relay::RelayResponseEnvelope {
        request_id: request.request_id.clone(),
        protocol_version: request.protocol_version,
        body: mj_core::relay::RelayResponseBody::Error {
            error: mj_core::relay::RelayProtocolError {
                code: mj_core::relay::RelayErrorCode::InvalidRequest,
                message: "unknown variant `release_checkpoint`".into(),
                retryable: false,
                detail: None,
            },
        },
    }
}
/// A relay target served by this test binary over stdio. Each start of the
/// server appends to `starts`, if given.
#[cfg(unix)]
pub(crate) fn latch_relay_target(
    relay_root: &Path,
    starts: Option<&Path>,
    release: ReleaseSupport,
    running: bool,
) -> crate::session_manager::RelaySessionTarget {
    // `RelayClient` parses every stdout line as JSON, so libtest's own
    // progress lines are dropped before they reach the protocol reader.
    let script = format!(
        "\"$0\" --exact {}::latch_relay_child_serves_stdio --nocapture | \
             grep --line-buffered '^{{'",
        module_path!()
            .strip_prefix("mj_controller::")
            .unwrap_or(module_path!())
    );
    let mut spec = CommandSpec::new(
        "sh",
        [
            "-c".to_owned(),
            script,
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ],
    )
    .purpose("test latch relay");
    spec.env.insert(
        LATCH_RELAY_ROOT.to_owned(),
        relay_root.to_string_lossy().into_owned(),
    );
    if let Some(starts) = starts {
        spec.env.insert(
            LATCH_RELAY_STARTS.to_owned(),
            starts.to_string_lossy().into_owned(),
        );
    }
    if std::env::var_os(LATCH_CHECKPOINT_ONLY).is_some() {
        spec.env.insert(LATCH_CHECKPOINT_ONLY.into(), "1".into());
    }
    if release == ReleaseSupport::Rejected {
        spec.env
            .insert(LATCH_RELAY_REJECT_RELEASE.to_owned(), "1".to_owned());
    }
    if running {
        spec.env
            .insert(LATCH_RELAY_RUNNING.to_owned(), "1".to_owned());
    }
    crate::session_manager::RelaySessionTarget {
        session_id: LATCH_RELAY_SESSION.to_owned(),
        spec,
        worker_recovery: None,
        project_memory: None,
    }
}
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

    let lease = handle.lease_connection().await.unwrap();
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
/// The session actor absorbs a returned connection on its own task, so the
/// first command after a latch ends may still be refused.
#[cfg(unix)]
async fn wait_until_the_actor_serves_again(handle: &ManagedSessionHandle) {
    for attempt in 0.. {
        if handle.sync_now().await.is_ok() {
            return;
        }
        assert!(attempt < 200, "the actor never took its connection back");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
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

    latched_checkpoint(
        relay,
        barrier_command_id,
        cursor,
        CheckpointCompletion::HeldBarrier,
    )
    .complete()
    .await
    .unwrap();
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
/// waiting there: the barrier ends, ACP dispatch resumes, and only the
/// recovery floor waits for the installed archive.
#[cfg(unix)]
#[tokio::test]
async fn releasing_a_checkpoint_after_capture_defers_only_the_recovery_floor() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(RELEASE_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::releasing_a_checkpoint_after_capture_defers_only_the_recovery_floor",
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
        "an exported archive that is not installed may not release journal history"
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
        if let Some(active) = snapshot.operational.active_prompt {
            dispatched = Some(active);
            break;
        }
        assert!(attempt < 200, "a released barrier still froze ACP dispatch");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(dispatched.is_some());

    // The archive is installed, so the relay may finally forget the history
    // it covers.
    latched_checkpoint(
        relay,
        barrier_command_id,
        cursor.clone(),
        CheckpointCompletion::ReleasedAfterCapture,
    )
    .complete()
    .await
    .unwrap();
    handle.sync_now().await.unwrap();
    let installed = handle
        .view()
        .snapshot
        .expect("the actor published the advanced recovery floor");
    assert_eq!(installed.operational.recovery_floor_ordinal, cursor.ordinal);
    assert_eq!(installed.operational.recovery_floor_digest, cursor.digest);
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
    latched_checkpoint(relay, barrier_command_id, cursor.clone(), completion)
        .complete()
        .await
        .unwrap();
    handle.sync_now().await.unwrap();
    let completed = handle
        .view()
        .snapshot
        .expect("the actor published the completed barrier");
    assert_eq!(completed.operational.checkpoint_barrier, None);
    assert_eq!(completed.operational.recovery_floor_ordinal, cursor.ordinal);
}
/// A caller that cannot install a latched archive has to cancel its
/// barrier. The latch is already back with the session actor, so the only
/// thing that ends the barrier is dropping the connection that opened it:
/// the worker cancels barriers whose connection disappears.
#[cfg(unix)]
#[tokio::test]
async fn abandoning_a_latched_checkpoint_drops_the_connection_that_opened_its_barrier() {
    // MJ_DATA_DIR is process-global, so run the database-backed half in an
    // exact child test instead of racing unrelated tests in this process.
    if std::env::var_os(ABANDON_TEST_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let test_name = format!(
            "{}::abandoning_a_latched_checkpoint_drops_the_connection_that_opened_its_barrier",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        IsolatedTest::new(test_name)
            .env(ABANDON_TEST_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
        return;
    }
    // Alone in this child process, so it installs the one writer.
    let _writer = crate::database::install_isolated_test_writer();

    // An abandoned barrier that never releases its connection would hang
    // the suite instead of failing it, so turn a stall into a hard error.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(120));
        eprintln!("an abandoned checkpoint never released its relay connection");
        std::process::exit(101);
    });

    let relay_root = tempfile::tempdir().unwrap();
    let start_log = tempfile::tempdir().unwrap();
    let start_log = start_log.path().join("relay-starts");
    let (_channels, handle, mut relay, barrier_command_id, cursor) = latch_a_live_checkpoint(
        relay_root.path(),
        Some(&start_log),
        ReleaseSupport::Supported,
        false,
    )
    .await;
    relay.end_latch();
    wait_until_the_actor_serves_again(&handle).await;
    assert_eq!(relay_starts(&start_log), 1);

    latched_checkpoint(
        relay,
        barrier_command_id,
        cursor,
        CheckpointCompletion::HeldBarrier,
    )
    .abandon(LATCH_RELAY_SESSION)
    .await;

    // The actor serves again, which proves the reclaimed lease was not
    // leaked, and it is talking to a new relay process, which proves the
    // connection that opened the barrier was dropped rather than handed
    // back alive.
    wait_until_the_actor_serves_again(&handle).await;
    assert_eq!(relay_starts(&start_log), 2);
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
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.refused(command)
        }

        fn execute_with_stdin(
            &self,
            command: &CommandSpec,
            _input: &mut (dyn std::io::Read + Send),
        ) -> Result<CommandOutput> {
            self.refused(command)
        }

        fn stage_started(&self, stage: ProvisionStage) {
            self.active_stages.lock().unwrap().push(stage);
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
            resumed: true,
        })
        .unwrap();
        seed.record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: Vec::new(),
        })
        .unwrap();
    }
    // The archive covers the fake runtime's SessionOpened and
    // SessionConfigured events, before any checkpoint bookkeeping.
    let checkpoint = write_checkpoint_gate_archive(&archive_directory, LATCH_RELAY_SESSION, 2);

    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target_template_id = "local".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: data_directory.join("workers").join(LATCH_RELAY_SESSION),
    });
    session.checkpoint = Some(checkpoint.clone());
    crate::database::save_session(&session).unwrap();

    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: profile_home,
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
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
            }],
        },
    );
    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(LATCH_RELAY_SESSION.into(), session)]),
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
        .checkpoint_session_latched(
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::HoldThroughClose,
            CheckpointExportPolicy::ReuseUnchangedArchive,
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
    latched.complete().await.unwrap();
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
    let mut connection = handle.lease_connection().await.unwrap();
    let before = connection.connection_mut().sync().await.unwrap();
    assert_eq!(before.operational.execution, RelayExecutionState::Running);
    connection.release();
    let deferred = controller
        .checkpoint_session_latched(
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::ReleaseAfterLatch,
            CheckpointExportPolicy::ReuseUnchangedArchive,
        )
        .await;
    assert!(
        matches!(deferred, Err(ref error) if error.downcast_ref::<CheckpointDeferred>().is_some())
    );
    wait_until_the_actor_serves_again(&handle).await;
    let mut connection = handle.lease_connection().await.unwrap();
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

    // Real session content, and the same policy has to export again.
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
        .checkpoint_session_latched(
            LATCH_RELAY_SESSION,
            &executor,
            Some(&channels.control),
            LatchExclusivity::HoldThroughClose,
            CheckpointExportPolicy::ReuseUnchangedArchive,
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
            .filter(|(stage, _)| *stage == ProvisionStage::RecoveryCopy)
            .collect::<Vec<_>>(),
        vec![
            (ProvisionStage::RecoveryCopy, true),
            (ProvisionStage::RecoveryCopy, false)
        ]
    );
    assert!(executor.active_stages.lock().unwrap().is_empty());
    assert!(checkpoint.archive_path.exists());
}
#[cfg(unix)]
fn relay_starts(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}
/// A latched checkpoint carrying a placeholder artifact. These tests
/// exercise its relay barrier, not the archive it names.
#[cfg(unix)]
fn latched_checkpoint(
    relay: ControllerRelayLease,
    barrier_command_id: String,
    cursor: RelayCursor,
    completion: CheckpointCompletion,
) -> LatchedCheckpoint {
    LatchedCheckpoint {
        artifact: CheckpointArtifact {
            metadata: CheckpointMetadata {
                archive_path: PathBuf::from("checkpoint.hel.zip"),
                sha256: "a".repeat(64),
                created_at: now(),
                event_frontier: cursor.ordinal,
            },
            native_session_id: "native-session".into(),
            event_frontier_digest: cursor.digest.clone(),
        },
        relay,
        barrier_command_id,
        cursor,
        completion,
    }
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
    verify_checkpoint_artifact(session_id, &artifact).unwrap();
    verify_installed_checkpoint_gate(session_id, &artifact.metadata).unwrap();
}
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
    let mut workspace = IdleWorkspaceLease::acquire(&handle, HarnessKind::Codex)
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
    let mut workspace = IdleWorkspaceLease::acquire(&handle, HarnessKind::Codex)
        .await
        .unwrap();
    let submitting = handle.clone();
    let mut prompt = tokio::spawn(async move {
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
        IdleWorkspaceLease::acquire(&handle, HarnessKind::Codex)
            .await
            .is_err(),
        "a queued or running prompt must prevent file injection"
    );
}

#[cfg(unix)]
const IN_PLACE_CLOSE_CHILD: &str = "MJ_TEST_IN_PLACE_CLOSE_CHILD";

/// A move that keeps its environment still needs a verified checkpoint and a
/// sealed relay. It must stop there: the record stays `Closing` with its
/// checkpoint and its target, and nothing tears the target down.
#[cfg(unix)]
#[tokio::test]
async fn an_in_place_move_close_seals_the_source_and_keeps_its_target() {
    if std::env::var_os(IN_PLACE_CLOSE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::an_in_place_move_close_seals_the_source_and_keeps_its_target",
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
    let relay_root = data_directory.join("relay");
    let profile_home = data_directory.join("profile");
    let archive_directory = data_directory.join("archives");
    for directory in [&relay_root, &profile_home, &archive_directory] {
        std::fs::create_dir_all(directory).unwrap();
    }
    // The archive covers exactly the two observations seeded below, so the
    // close latch reuses it instead of exporting a new one.
    let mut seed =
        mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    seed.record_observation(mj_core::relay::RelayObservation::SessionOpened {
        native_session_id: "native-session".into(),
        native_continuity_lost: false,
        resumed: true,
    })
    .unwrap();
    seed.record_observation(mj_core::relay::RelayObservation::SessionConfigured {
        config_options: Vec::new(),
    })
    .unwrap();
    drop(seed);
    let checkpoint = write_checkpoint_gate_archive(&archive_directory, LATCH_RELAY_SESSION, 2);

    // A container is the case an in-place move is really for: the container,
    // its workspace, and its caches all survive the harness swap.
    let container_id = targets::resource_name(LATCH_RELAY_SESSION).unwrap();
    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.target_template_id = "podman".into();
    session.target = Some(TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: container_id.clone(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: format!("{container_id}-workspace"),
        },
    });
    session.checkpoint = Some(checkpoint.clone());
    crate::database::save_session(&session).unwrap();

    // `validate_move_checkpoint` re-reads the configuration from disk and
    // compares its fingerprint, so the controller's config has to be the
    // persisted one.
    let (config, ()) = Config::update(|config| {
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: profile_home.clone(),
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        config.targets.insert(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: mj_core::config::ContainerTemplate {
                    build_cache: None,
                    image: "test:latest".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: mj_core::config::PodmanWorkspaceStorage::PodmanVolume,
                },
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
                }],
            },
        );
        Ok(())
    })
    .unwrap();

    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(LATCH_RELAY_SESSION.into(), session.clone())]),
            ..State::default()
        },
    };

    let selection = mj_core::state::MoveSelection {
        clear_resource_allocation: false,
        session_id: LATCH_RELAY_SESSION.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("podman".into()),
        additional_mounts: Some(Vec::new()),
        resource_allocation: None,
    };
    let mut operation = mj_core::state::MoveOperation {
        in_place: true,
        source_checkpoint_only: false,
        operation_id: "move-in-place-close".into(),
        selection: selection.clone(),
        source_profile_id: "codex".into(),
        source_target_template_id: "podman".into(),
        source_target: session.target.clone(),
        source_native_session_id: session.native_session_id.clone(),
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
        phase: mj_core::state::MovePhase::ClosingSource,
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
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.purposes.lock().unwrap().push(command.purpose.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
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
            LATCH_RELAY_SESSION,
            &executor,
            &channels.control,
            &mut operation,
            None,
            crate::controller::lifecycle::SourceTargetDisposition::RetainForInPlaceSwap,
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
    assert_eq!(sealed.checkpoint.as_ref(), Some(&checkpoint));
    assert_eq!(sealed.target, session.target);
    assert_eq!(operation.checkpoint.as_ref(), Some(&checkpoint));
    // The durable record has to agree: recovery reads it, not this process.
    let persisted = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(persisted.state, SessionState::Closing);
    assert_eq!(persisted.target, session.target);
    assert!(persisted.checkpoint.is_some());

    let purposes = executor.purposes.lock().unwrap().clone();
    for purpose in &purposes {
        let lowered = purpose.to_lowercase();
        assert!(
            !lowered.contains("remove")
                && !lowered.contains("stop")
                && !lowered.contains("delete")
                && !lowered.contains("clean"),
            "an in-place close must not tear anything down, but it ran {purpose:?} \
             (all: {purposes:?})"
        );
    }
    assert!(
        !purposes.iter().any(|purpose| purpose.contains("podman")),
        "no container command ran at all: {purposes:?}"
    );
}

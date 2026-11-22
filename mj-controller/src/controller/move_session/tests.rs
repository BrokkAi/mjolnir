//! Milestone 5 Move executor tests:
//! - Same-target in-place Move (Slice 5A, Relay Protocol 81)
//! - Cross-target Move ownership commit point (`DestinationOwns`) & post-commit
//!   source retirement
//! - Legacy (pre-148) `MoveOperation` upgrade recovery (source rollback &
//!   destination finish-forward including `ResumingDestination` + `Running`
//!   crash window)
//! - `QueuePolicy::Suppress` & `.mj/keep/` `--skip` transfer verification

use std::path::PathBuf;

use mj_core::state::{
    LegacyMoveOperation, LegacyMovePhase, MoveOperation, MovePhase, MoveSelection,
    ResumeQueueDisposition, SessionRecord, SessionState, StoredMoveOperation, TargetLocator,
    TransferRoute,
};

use super::in_place::{deferred_environment_change_notice, ensure_worker_supports_in_place_move};
use super::recovery::{
    MoveOwnership, classify_legacy_move_ownership, classify_move_ownership,
    legacy_source_rollback_notice,
};
use super::transport::{KeepTransferOptions, preview_transfer_route, targets_share_host};

fn sample_target(name: &str) -> TargetLocator {
    TargetLocator::LocalBare {
        worker_root: PathBuf::from(format!("/tmp/mj-move-test-{name}")),
    }
}

fn sample_session_record(state: SessionState, target: TargetLocator) -> SessionRecord {
    let mut record: SessionRecord = serde_json::from_str(
        r#"{"id":"sess-move-test","title":"move test","bundle_id":"default","harness_kind":"codex","last_profile":"dest-profile","target_template_id":"local-dest","state":"stopped","created_at":"2026-11-21T00:00:00Z","updated_at":"2026-11-21T00:00:00Z"}"#,
    )
    .unwrap();
    record.state = state;
    record.target = Some(Box::new(target));
    record.native_session_id = Some("native-1".to_owned());
    record
}

#[test]
fn legacy_resuming_destination_running_crash_window_finishes_forward() {
    let dest_target = sample_target("dest");
    let session = sample_session_record(SessionState::Running, dest_target.clone());
    let legacy = LegacyMoveOperation {
        selection: None,
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        acknowledge_interruption: false,
        handoff: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "legacy-crash-window".to_owned(),
        source_profile_id: "src-profile".to_owned(),
        source_target_template_id: "local-src".to_owned(),
        source_target: Some(sample_target("src")),
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: Some(dest_target),
        destination_native_session_id: Some("native-1".to_owned()),
        destination_store_id: Some("store-dest".to_owned()),
        configuration_fingerprint: String::new(),
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Start,
        phase: LegacyMovePhase::ResumingDestination,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-11-21T00:00:00Z".to_owned(),
        updated_at: "2026-11-21T00:00:00Z".to_owned(),
        error: None,
    };

    assert_eq!(
        classify_legacy_move_ownership(&legacy, Some(&session)),
        MoveOwnership::Destination
    );

    // When session is NOT running on destination, source still owns and rolls back.
    let stopped = sample_session_record(SessionState::Stopped, sample_target("src"));
    assert_eq!(
        classify_legacy_move_ownership(&legacy, Some(&stopped)),
        MoveOwnership::Source
    );
    let notice = legacy_source_rollback_notice(&legacy.operation_id);
    assert!(notice.contains("mj move"), "{notice}");

    // Legacy JSON decodes via StoredMoveOperation::Legacy.
    let json = serde_json::to_string(&legacy).unwrap();
    assert!(matches!(
        StoredMoveOperation::decode_json(&json).unwrap(),
        StoredMoveOperation::Legacy(_)
    ));
}

// Hard-won: a35beec20: a sealed source could not retry with a replacement delegation policy.
#[test]
fn cross_target_move_route_preview_and_commit_point_ownership() {
    let local_a = mj_core::targets::TargetLocator::LocalBare {
        worker_root: "/tmp/a".to_owned(),
    };
    let local_b = mj_core::targets::TargetLocator::LocalPodman {
        container_id: "mj-b".to_owned(),
        workspace_storage: mj_core::targets::PodmanWorkspaceLocator::Volume {
            name: "mj-vol-b".to_owned(),
        },
        borrowed_from: None,
    };
    assert!(targets_share_host(&local_a, &local_b));
    assert_eq!(
        preview_transfer_route(&local_a, &local_b),
        mj_core::state::RoutePreview::SameHost
    );

    let state_a = TargetLocator::LocalBare {
        worker_root: PathBuf::from("/tmp/a"),
    };
    let state_b = TargetLocator::LocalPodman {
        container_id: "mj-b".to_owned(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::Volume {
            name: "mj-vol-b".to_owned(),
        },
        borrowed_from: None,
    };

    let keep_opts = KeepTransferOptions {
        keep_dir: Some(PathBuf::from("/src/.mj/keep/s1")),
        keep_dest: Some(PathBuf::from("/dst/.mj/keep/s1")),
        skip: vec![PathBuf::from("cache/big.bin")],
    };
    assert_eq!(keep_opts.skip, vec![PathBuf::from("cache/big.bin")]);

    let mut op = MoveOperation {
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        transfer_route: Some(TransferRoute::SameHost),
        consent: Default::default(),
        skip_keep_paths: vec![PathBuf::from("cache/big.bin")],
        granted_keys: Vec::new(),
        granted_ec2_rules: Vec::new(),
        in_place_expected_store_id: None,
        in_place_continuation_command_id: None,
        in_place_held_command_ids: Vec::new(),
        in_place_harness_restarted: false,
        deferred_environment_changes: Vec::new(),
        source_snapshot: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "op-cross-1".to_owned(),
        selection: MoveSelection {
            consent: Default::default(),
            skip_keep_paths: vec![PathBuf::from("cache/big.bin")],
            clear_resource_allocation: false,
            session_id: "s1".to_owned(),
            profile_id: Some("p2".to_owned()),
            target_template_id: Some("podman".to_owned()),
            additional_mounts: None,
            resource_allocation: None,
            subagents: None,
            oracle: None,
            supervisors: None,
            oracle_choice: None,
        },
        source_profile_id: "p1".to_owned(),
        source_target_template_id: "local".to_owned(),
        source_target: Some(state_a),
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: Some(state_b),
        destination_native_session_id: None,
        destination_store_id: Some("store-dest".to_owned()),
        configuration_fingerprint: String::new(),
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Discard,
        phase: MovePhase::DestinationStarting,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-11-21T00:00:00Z".to_owned(),
        updated_at: "2026-11-21T00:00:00Z".to_owned(),
        error: None,
    };

    // Crash right before DestinationOwns: source owns and retains its environment.
    assert_eq!(classify_move_ownership(&op), MoveOwnership::Source);
    assert!(op.retains_source_environment());
    let mut failed = op.clone();
    failed.phase = MovePhase::Failed;
    failed.destination_target = None;
    failed.recovery_session = Some(sample_session_record(
        SessionState::Closing,
        failed.source_target.clone().unwrap(),
    ));
    let archives = tempfile::tempdir().unwrap();
    failed.source_snapshot = Some(Box::new(mj_core::state::MoveSourceSnapshot {
        archive_path: archives.path().join("lost.hel.zip"),
        sha256: "old-digest".into(),
        manifest: None,
        queued_prompts: Vec::new(),
        mailbox: None,
    }));
    let sealed = failed.recovery_session.as_ref().unwrap();
    let mut reopened = sealed.clone();
    reopened.state = SessionState::Running;
    reopened.last_profile = failed.source_profile_id.clone();
    assert!(
        super::retained_move_snapshot_is_missing(
            &failed,
            &mj_core::config::Config::default(),
            &crate::targets::ProcessExecutor,
        )
        .unwrap()
    );
    assert!(!super::move_needs_fresh_capture(&failed, sealed));
    assert!(super::move_needs_fresh_capture(&failed, &reopened));
    let mut retry = failed.selection.clone();
    retry.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: Some(Box::new(mj_core::config::SubagentChoice {
            model: "available-model".into(),
            effort: None,
        })),
        subtle: None,
    });
    assert!(super::matches_move_retry_selection(&failed, &retry));
    let mut different_target = retry.clone();
    different_target.target_template_id = Some("another-host".into());
    assert!(!super::matches_move_retry_selection(
        &failed,
        &different_target
    ));
    failed.destination_target = op.destination_target.clone();
    assert!(!super::matches_move_retry_selection(&failed, &retry));
    failed.destination_target = None;
    failed.queue_admission_started = true;
    assert!(!super::matches_move_retry_selection(&failed, &retry));
    assert!(!super::move_needs_fresh_capture(&failed, &reopened));
    // Crash right after DestinationOwns: destination owns, finish forward.
    op.phase = MovePhase::DestinationOwns;
    assert_eq!(classify_move_ownership(&op), MoveOwnership::Destination);
    assert!(!op.retains_source_environment());
    op.phase = MovePhase::SourceRetired;
    assert_eq!(classify_move_ownership(&op), MoveOwnership::Destination);
    assert!(!op.retains_source_environment());
}

#[test]
fn in_place_protocol_gate_and_deferred_notice_contract() {
    assert!(ensure_worker_supports_in_place_move("s1", 81, false).is_ok());
    let manual = ensure_worker_supports_in_place_move("s1", 80, false)
        .unwrap_err()
        .to_string();
    assert!(manual.contains("mj restart --session s1"), "{manual}");
    assert!(manual.contains("Restart agent"), "{manual}");
    let quota = ensure_worker_supports_in_place_move("s1", 80, true)
        .unwrap_err()
        .to_string();
    assert!(quota.contains("Waiting for quota reset"), "{quota}");
    assert_eq!(
        deferred_environment_change_notice("Container mounts"),
        "Container mounts takes effect when this session's environment is next recreated"
    );
}

#[test]
fn recover_legacy_move_operation_probes_dead_source_and_handles_stopped_checkpoint() {
    let name = crate::controller::test_support::test_name(
        module_path!(),
        "recover_legacy_move_operation_probes_dead_source_and_handles_stopped_checkpoint",
    );
    if !super::test_fixtures::isolated_test_child(&name, "MJ_LEGACY_MOVE_RECOVERY_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let dir = tempfile::tempdir().unwrap();
    let dead_root = dir.path().join("sess-dead");
    std::fs::create_dir_all(&dead_root).unwrap();

    // Case 1: Source-owned legacy move where the source worker is dead/missing
    // and there is no checkpoint -> must settle as Error (never Running) and
    // delete the legacy session_moves row.
    let mut dead_session = crate::controller::test_support::checkpoint_test_session("sess-dead");
    dead_session.state = SessionState::Closing;
    dead_session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: dead_root.clone(),
    }));
    dead_session.checkpoint = None;
    crate::database::save_session(&dead_session).unwrap();

    let dead_legacy = LegacyMoveOperation {
        selection: None,
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        acknowledge_interruption: false,
        handoff: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "legacy-dead-source".into(),
        source_profile_id: dead_session.last_profile.clone(),
        source_target_template_id: dead_session.target_template_id.clone(),
        source_target: dead_session.target.clone().map(|v| *v),
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: String::new(),
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Start,
        phase: LegacyMovePhase::ClosingSource,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-11-21T00:00:00Z".into(),
        updated_at: "2026-11-21T00:00:00Z".into(),
        error: None,
    };
    let mut controller = crate::controller::Controller {
        config: mj_core::config::Config::default(),
        state: mj_core::state::State {
            sessions: [("sess-dead".into(), dead_session.clone())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        controller
            .recover_legacy_move_operation(
                "sess-dead",
                &dead_legacy,
                &crate::targets::ProcessExecutor,
            )
            .await
            .unwrap();
    });
    assert_eq!(
        controller.state.sessions["sess-dead"].state,
        SessionState::Error
    );
    assert!(
        crate::database::load_legacy_move_operations()
            .unwrap()
            .is_empty()
    );

    // Case 2: Source-owned legacy move where target is already gone and a
    // checkpoint exists -> settles cleanly as Stopped.
    let mut stopped_session =
        crate::controller::test_support::checkpoint_test_session("sess-stopped");
    stopped_session.state = SessionState::Closing;
    stopped_session.target = None;
    stopped_session.checkpoint = Some(Box::new(mj_core::state::CheckpointMetadata {
        archive_path: dir.path().join("kept.hel.zip"),
        sha256: "a".repeat(64),
        created_at: "2026-11-21T00:00:00Z".into(),
        event_frontier: 1,
    }));
    crate::database::save_session(&stopped_session).unwrap();
    controller
        .state
        .sessions
        .insert("sess-stopped".into(), stopped_session.clone());
    let stopped_legacy = LegacyMoveOperation {
        selection: None,
        quota_stop_reason: None,
        quota_continuation: None,
        prepared_destination: None,
        acknowledge_interruption: false,
        handoff: None,
        source_checkpoint_only: false,
        in_place: false,
        operation_id: "legacy-stopped-source".into(),
        source_profile_id: stopped_session.last_profile.clone(),
        source_target_template_id: stopped_session.target_template_id.clone(),
        source_target: None,
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: String::new(),
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Start,
        phase: LegacyMovePhase::ClosingSource,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-11-21T00:00:00Z".into(),
        updated_at: "2026-11-21T00:00:00Z".into(),
        error: None,
    };
    runtime.block_on(async {
        controller
            .recover_legacy_move_operation(
                "sess-stopped",
                &stopped_legacy,
                &crate::targets::ProcessExecutor,
            )
            .await
            .unwrap();
    });
    assert_eq!(
        controller.state.sessions["sess-stopped"].state,
        SessionState::Stopped
    );
}

// Hard-won: #1631: retrying a partially started Move without --queue defaulted to Discard and was refused.
#[cfg(unix)]
#[test]
fn retrying_partially_started_move_without_queue_preserves_original_choice() {
    let name = crate::controller::test_support::test_name(
        module_path!(),
        "retrying_partially_started_move_without_queue_preserves_original_choice",
    );
    if std::env::var_os("MJ_RETRY_LOCKED_QUEUE_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        crate::controller::test_support::IsolatedTest::new(name)
            .env("MJ_RETRY_LOCKED_QUEUE_CHILD", "1")
            .env(
                "MJ_WORKER_BINARY",
                super::test_fixtures::fake_worker_dispatcher(),
            )
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let source_home = dir.path().join("source-home");
    let dest_home = dir.path().join("dest-home");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::create_dir_all(&source_home).unwrap();
    std::fs::create_dir_all(&dest_home).unwrap();
    let mut config = mj_core::config::Config::default();
    config.profiles.insert(
        "source".into(),
        mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Claude, source_home),
    );
    config.profiles.insert(
        "destination".into(),
        mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Claude, dest_home),
    );
    config.targets.insert(
        "local-bare".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    let mut session = crate::controller::test_support::checkpoint_test_session("sess-locked-queue");
    session.harness_kind = mj_core::config::HarnessKind::Claude;
    session.state = SessionState::Stopped;
    session.last_profile = "source".into();
    session.target_template_id = "local-bare".into();
    session.target = None;
    session.project_directory = Some(checkout);
    crate::database::save_session(&session).unwrap();
    let mut controller = crate::controller::Controller {
        config,
        state: mj_core::state::State::default(),
    };
    controller
        .state
        .sessions
        .insert(session.id.clone(), session.clone());

    let mut failed = super::test_fixtures::source_recovery_operation(&session);
    failed.phase = MovePhase::Failed;
    failed.queue = ResumeQueueDisposition::Start;
    failed.queue_admission_started = true;
    failed.queue_admission_finished = false;
    failed.source_snapshot = Some(Box::new(mj_core::state::MoveSourceSnapshot {
        archive_path: dir.path().join("missing-snapshot.hel.zip"),
        sha256: "a".repeat(64),
        manifest: None,
        queued_prompts: Vec::new(),
        mailbox: None,
    }));
    let catalog =
        crate::profile_catalog::ProfileCatalog::new(tokio_util::sync::CancellationToken::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let executor = crate::targets::ProcessExecutor;
        let admission = crate::controller::test_support::session_admission(&session.id);

        // 1) Empty captured queue: explicit conflicting choice is still refused;
        //    omitted queue inherits the locked Start choice.
        crate::database::save_move_operation(&failed).unwrap();
        let preparation = controller
            .prepare_move_session_controlled(&catalog, failed.selection.clone(), &executor)
            .await
            .unwrap();
        assert!(preparation.queued_commands.is_empty());

        let rejected_accepted = std::sync::atomic::AtomicBool::new(false);
        let mismatch = controller
            .move_session_managed_controlled_with_admission(
                &admission,
                &catalog,
                mj_core::state::MoveSessionRequest {
                    preparation: preparation.clone(),
                    queue: Some(ResumeQueueDisposition::Discard),
                    acknowledge_interruption: true,
                },
                None,
                &executor,
                &channels.control,
                || rejected_accepted.store(true, std::sync::atomic::Ordering::SeqCst),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{mismatch:#}").contains("queued work may already have run"),
            "{mismatch:#}"
        );
        assert!(!rejected_accepted.load(std::sync::atomic::Ordering::SeqCst));

        let omitted_accepted = std::sync::atomic::AtomicBool::new(false);
        let _ = controller
            .move_session_managed_controlled_with_admission(
                &admission,
                &catalog,
                mj_core::state::MoveSessionRequest {
                    preparation,
                    queue: None,
                    acknowledge_interruption: true,
                },
                None,
                &executor,
                &channels.control,
                || {
                    omitted_accepted.store(true, std::sync::atomic::Ordering::SeqCst);
                    let stored = crate::database::load_move_operation(&session.id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(stored.queue, ResumeQueueDisposition::Start);
                },
            )
            .await;
        assert!(
            omitted_accepted.load(std::sync::atomic::Ordering::SeqCst),
            "omitting queue on a locked retry should inherit the original Start choice"
        );

        // 2) Non-empty captured queue: omitting queue on a locked retry also
        //    inherits Start without demanding a redundant queue choice.
        failed
            .source_snapshot
            .as_deref_mut()
            .unwrap()
            .queued_prompts
            .push(mj_core::archive::CanonicalQueuedPrompt {
                command_id: "queued-1".into(),
                kind: mj_core::archive::CanonicalQueuedCommandKind::Prompt,
                content: vec![serde_json::json!({"type": "text", "text": "continue"})],
                queued_at_ms: 1,
            });
        crate::database::save_move_operation(&failed).unwrap();
        let preparation = controller
            .prepare_move_session_controlled(&catalog, failed.selection.clone(), &executor)
            .await
            .unwrap();
        assert_eq!(preparation.queued_commands.len(), 1);
        let queued_omitted_accepted = std::sync::atomic::AtomicBool::new(false);
        let _ = controller
            .move_session_managed_controlled_with_admission(
                &admission,
                &catalog,
                mj_core::state::MoveSessionRequest {
                    preparation,
                    queue: None,
                    acknowledge_interruption: true,
                },
                None,
                &executor,
                &channels.control,
                || {
                    queued_omitted_accepted.store(true, std::sync::atomic::Ordering::SeqCst);
                    let stored = crate::database::load_move_operation(&session.id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(stored.queue, ResumeQueueDisposition::Start);
                },
            )
            .await;
        assert!(
            queued_omitted_accepted.load(std::sync::atomic::Ordering::SeqCst),
            "omitting queue on a locked retry with captured prompts should inherit Start"
        );

        channels.shutdown.shutdown().await.unwrap();
    });
}

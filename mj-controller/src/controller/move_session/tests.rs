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
        orchestrators: false,
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
    assert!(ensure_worker_supports_in_place_move("s1", 91, false).is_ok());
    let manual = ensure_worker_supports_in_place_move("s1", 90, false)
        .unwrap_err()
        .to_string();
    assert!(manual.contains("mj restart --session s1"), "{manual}");
    assert!(manual.contains("Restart agent"), "{manual}");
    let quota = ensure_worker_supports_in_place_move("s1", 90, true)
        .unwrap_err()
        .to_string();
    assert!(quota.contains("Waiting for quota reset"), "{quota}");
    assert_eq!(
        deferred_environment_change_notice("Container mounts"),
        "Container mounts takes effect when this session's environment is next recreated"
    );
}

#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
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

#[cfg(unix)]
#[test]
fn preparing_cross_harness_move_without_utility_profiles_succeeds() {
    let name = crate::controller::test_support::test_name(
        module_path!(),
        "preparing_cross_harness_move_without_utility_profiles_succeeds",
    );
    if !super::test_fixtures::isolated_test_child(&name, "MJ_MOVE_NO_UTILITY_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let source_home = dir.path().join("source-home");
    let destination_home = dir.path().join("destination-home");
    for path in [&checkout, &source_home, &destination_home] {
        std::fs::create_dir_all(path).unwrap();
    }

    let mut config = mj_core::config::Config::default();
    config.profiles.clear();
    config.profiles.insert(
        "source".into(),
        mj_core::config::HarnessProfile::new(mj_core::config::HarnessKind::Claude, source_home),
    );
    config.profiles.insert(
        "destination".into(),
        mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::OpenCode,
            destination_home,
        ),
    );
    config.targets.insert(
        "local-bare".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );

    let mut session = crate::controller::test_support::checkpoint_test_session("sess-no-utility");
    session.harness_kind = mj_core::config::HarnessKind::Claude;
    session.state = SessionState::Running;
    session.last_profile = "source".into();
    session.target_template_id = "local-bare".into();
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: dir.path().join("worker").join(&session.id),
    }));
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
    controller
        .state
        .environment_owners
        .insert(session.id.clone(), session.id.clone());

    let catalog =
        crate::profile_catalog::ProfileCatalog::new(tokio_util::sync::CancellationToken::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let selection = MoveSelection {
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
            clear_resource_allocation: false,
            session_id: session.id.clone(),
            profile_id: Some("destination".into()),
            target_template_id: None,
            additional_mounts: None,
            resource_allocation: None,
            subagents: None,
            oracle: None,
            oracle_choice: None,
        };
        let preparation = controller
            .prepare_move_session_controlled(&catalog, selection, &crate::targets::ProcessExecutor)
            .await
            .unwrap();
        assert!(preparation.cross_harness);
        assert!(preparation.in_place);
    });
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

    controller
        .state
        .environment_owners
        .insert(session.id.clone(), session.id.clone());

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
        assert_eq!(
            preparation.locked_queue,
            Some(ResumeQueueDisposition::Start)
        );

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
        assert_eq!(
            preparation.locked_queue,
            Some(ResumeQueueDisposition::Start)
        );
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

// Hard-won: #1639: cross-target Move held daemon upgrade admission across resumable phases.
#[cfg(unix)]
#[test]
fn cross_target_move_yields_handoff_admission_during_source_stop_and_transfer() {
    let name = crate::controller::test_support::test_name(
        module_path!(),
        "cross_target_move_yields_handoff_admission_during_source_stop_and_transfer",
    );
    if !super::test_fixtures::isolated_test_child(&name, "MJ_MOVE_HANDOFF_YIELD_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let source_home = dir.path().join("source-home");
    let dest_home = dir.path().join("dest-home");
    for path in [&checkout, &source_home, &dest_home] {
        std::fs::create_dir_all(path).unwrap();
    }
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

    let mut session = crate::controller::test_support::checkpoint_test_session("sess-move-yield");
    session.harness_kind = mj_core::config::HarnessKind::Claude;
    session.state = SessionState::Running;
    session.last_profile = "source".into();
    session.target_template_id = "local-bare".into();
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

    let mut operation = super::test_fixtures::source_recovery_operation(&session);
    operation.in_place = false;
    operation.phase = MovePhase::Capturing;
    crate::database::save_move_operation(&operation).unwrap();

    struct HandoffObservingExecutor {
        gate: std::sync::Arc<crate::upgrade::Gate>,
        work: std::sync::Mutex<Option<crate::upgrade::Work>>,
        shutdown: std::sync::atomic::AtomicBool,
        observed_labels: std::sync::Mutex<Vec<Vec<String>>>,
        close_on_stop_children: bool,
    }

    impl crate::targets::CommandExecutor for HandoffObservingExecutor {
        fn execute(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.observed_labels
                .lock()
                .unwrap()
                .push(self.gate.active_labels());
            anyhow::bail!("stopped in test at {}", command.purpose)
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
            _input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }
        fn cancellation_requested_without_deadline(&self) -> bool {
            self.shutdown_interrupted()
        }
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }
        fn begin_resumable_work(&self) -> anyhow::Result<()> {
            self.work.lock().unwrap().take();
            Ok(())
        }
        fn end_resumable_work(&self, label: &'static str) -> anyhow::Result<()> {
            match self.gate.enter_after_draining(label) {
                Ok(work) => {
                    *self.work.lock().unwrap() = Some(work);
                    Ok(())
                }
                Err(error) => {
                    self.shutdown
                        .store(true, std::sync::atomic::Ordering::Release);
                    Err(error)
                }
            }
        }
        fn shutdown_interrupted(&self) -> bool {
            self.shutdown.load(std::sync::atomic::Ordering::Acquire)
        }
        fn stop_borrowed_workers_before_quiesce(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            Box::pin(async move {
                self.observed_labels
                    .lock()
                    .unwrap()
                    .push(self.gate.active_labels());
                if self.close_on_stop_children {
                    // Gate must be empty so try_close succeeds immediately.
                    assert!(
                        self.gate.try_close(),
                        "stop_borrowed_workers_before_quiesce still held handoff admission"
                    );
                }
                Ok(())
            })
        }
    }

    let gate = std::sync::Arc::new(crate::upgrade::Gate::default());
    let work = gate.enter("session lifecycle").unwrap();
    let executor = HandoffObservingExecutor {
        gate: gate.clone(),
        work: std::sync::Mutex::new(Some(work)),
        shutdown: std::sync::atomic::AtomicBool::new(false),
        observed_labels: std::sync::Mutex::new(Vec::new()),
        close_on_stop_children: true,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let admission = crate::controller::test_support::session_admission(&session.id);

        // Handoff closes the gate during stop_borrowed_workers_before_quiesce without waiting;
        // execute_move fails on end_resumable_work, and finish_move_result
        // preserves the durable Move as "interrupted" instead of Failed.
        let result = controller
            .execute_move(
                &admission,
                &mut operation,
                None,
                &executor,
                &channels.control,
                super::MoveSourceRelay::default(),
            )
            .await;
        assert!(result.is_err());
        assert!(executor.shutdown.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            executor.observed_labels.lock().unwrap().as_slice(),
            &[Vec::<String>::new()]
        );

        // finish_move_result preserves the durable Move as "interrupted" when
        // shutdown_interrupted() is set even if the process-global gate is open.
        let outcome = controller
            .finish_move_result(&mut operation, result, &executor)
            .unwrap();
        assert_eq!(outcome.outcome, "interrupted", "{outcome:?}");
        let stored = crate::database::load_move_operation(&session.id)
            .unwrap()
            .unwrap();
        assert!(
            stored.is_active(),
            "interrupted cross-target Move must remain active for recovery: {stored:?}"
        );

        // Next daemon's startup recovery resumes the interrupted Capturing Move
        // by re-entering execute_move (running stop_borrowed_workers_before_quiesce again)
        // instead of failing and demanding an explicit retry.
        let gate2 = std::sync::Arc::new(crate::upgrade::Gate::default());
        let work2 = gate2.enter("session lifecycle").unwrap();
        let recovery_executor = HandoffObservingExecutor {
            gate: gate2,
            work: std::sync::Mutex::new(Some(work2)),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            observed_labels: std::sync::Mutex::new(Vec::new()),
            close_on_stop_children: true,
        };
        let recovered = controller
            .recover_move_managed_controlled(
                &admission,
                stored,
                &recovery_executor,
                &channels.control,
            )
            .await
            .unwrap();
        assert_eq!(recovered.outcome, "interrupted", "{recovered:?}");
        assert_eq!(
            recovery_executor.observed_labels.lock().unwrap().as_slice(),
            &[Vec::<String>::new()],
            "recovery must resume execute_move through stop_borrowed_workers_before_quiesce"
        );

        // An interrupted Transferring cross-target Move (with Stopped source
        // and retained snapshot) also resumes transfer on recovery instead of
        // rolling back to Failed, while a user-cancelled one still rolls back.
        let mut stopped = session.clone();
        stopped.state = SessionState::Stopped;
        stopped.target = None;
        crate::database::save_session(&stopped).unwrap();
        controller
            .state
            .sessions
            .insert(stopped.id.clone(), stopped.clone());
        let mut transferring = super::test_fixtures::source_recovery_operation(&stopped);
        transferring.in_place = false;
        transferring.phase = MovePhase::Transferring;
        transferring.recovery_session = Some(stopped.clone());
        transferring.source_snapshot = Some(Box::new(mj_core::state::MoveSourceSnapshot {
            archive_path: dir.path().join("missing-snapshot.hel.zip"),
            sha256: "a".repeat(64),
            manifest: None,
            queued_prompts: Vec::new(),
            mailbox: None,
        }));
        crate::database::save_move_operation(&transferring).unwrap();
        let gate3 = std::sync::Arc::new(crate::upgrade::Gate::default());
        let work3 = gate3.enter("session lifecycle").unwrap();
        let transfer_executor = HandoffObservingExecutor {
            gate: gate3,
            work: std::sync::Mutex::new(Some(work3)),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            observed_labels: std::sync::Mutex::new(Vec::new()),
            close_on_stop_children: false,
        };
        let resumed_transfer = controller
            .recover_move_managed_controlled(
                &admission,
                transferring.clone(),
                &transfer_executor,
                &channels.control,
            )
            .await
            .unwrap();
        assert!(
            !resumed_transfer
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("destination restoration was interrupted"),
            "non-cancelled Transferring recovery must continue into execute_move rather than rolling back: {resumed_transfer:?}"
        );

        transferring.cancellation_requested = true;
        crate::database::save_move_operation(&transferring).unwrap();
        let cancelled_transfer = controller
            .recover_move_managed_controlled(
                &admission,
                transferring,
                &transfer_executor,
                &channels.control,
            )
            .await
            .unwrap();
        assert_eq!(
            cancelled_transfer.outcome, "cancelled",
            "{cancelled_transfer:?}"
        );

        channels.shutdown.shutdown().await.unwrap();
    });
}

#[cfg(unix)]
#[tokio::test]
async fn recovering_a_ready_cross_target_move_retires_closed_source_transcripts() {
    const NAME: &str = "recovering_a_ready_cross_target_move_retires_closed_source_transcripts";
    let name = crate::controller::test_support::test_name(module_path!(), NAME);
    if !super::test_fixtures::isolated_test_child(&name, "MJ_MOVE_CLOSED_CHILD_RECOVERY") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = tempfile::tempdir().unwrap();
    let session_id = "recovering-move-parent";
    let mut parent = crate::controller::test_support::checkpoint_test_session(session_id);
    parent.state = SessionState::Running;
    parent.last_profile = "source".into();
    parent.target_template_id = "local-bare".into();
    // This source has a registered environment and no worker to retire.
    parent.target = None;
    parent.checkpoint = None;
    crate::database::save_session(&parent).unwrap();
    let register = |id: &str| {
        let mut record = parent.clone();
        record.id = id.into();
        record.state = SessionState::Stopped;
        let relation = serde_json::from_value(serde_json::json!({
            "child_session_id": id, "parent_session_id": session_id, "task_name": "test",
            "profile_id": "source", "working_directory": "", "initial_prompt": "test",
            "request_key": id, "created_at": parent.created_at
        }))
        .unwrap();
        crate::database::save_subagent_session(&record, &relation).unwrap();
    };
    register("source-borrower");
    let mut operation = super::test_fixtures::source_recovery_operation(&parent);
    operation.phase = MovePhase::DestinationStarting;
    crate::database::save_move_operation(&operation).unwrap();

    let mut destination = parent.clone();
    destination.last_profile = "destination".into();
    destination.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.path().join(session_id),
    }));
    // Reserve a distinct destination using the same environment transition
    // as resuming a reclaimed source with a new worker placement.
    let source = crate::database::session_environment(session_id)
        .unwrap()
        .unwrap();
    crate::database::begin_environment_retirement(&source.id).unwrap();
    crate::database::finish_environment_retirement(&source.id).unwrap();
    destination.state = SessionState::Provisioning;
    crate::database::save_session(&destination).unwrap();
    destination.state = SessionState::Running;
    crate::database::save_lifecycle_session(&destination).unwrap();
    register("destination-borrower");
    assert_ne!(
        crate::database::session_environment(session_id)
            .unwrap()
            .unwrap()
            .id,
        source.id
    );

    let mut controller = crate::controller::Controller::load().unwrap();
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    let admission = crate::controller::test_support::session_admission(session_id);
    // Recovery will fail at queue admission because there is no relay fixture
    // on the ready destination. The ownership cut precedes queue admission.
    controller
        .recover_move_managed_controlled(
            &admission,
            operation,
            &crate::targets::ProcessExecutor,
            &channels.control,
        )
        .await
        .unwrap();
    channels.shutdown.shutdown().await.unwrap();
    let operation = crate::database::load_move_operation(session_id)
        .unwrap()
        .unwrap();
    assert!(operation.queue_admission_started, "{operation:?}");
    assert!(
        crate::database::load_session_record("source-borrower")
            .unwrap()
            .is_none()
    );
    assert!(
        crate::database::load_session_record("destination-borrower")
            .unwrap()
            .is_some()
    );
}

#[test]
fn parent_move_reports_only_workers_in_its_environment() {
    use mj_core::state::{SessionState, State};
    use mj_core::subagent::{SubagentKind, SubagentRecord};

    let mut state = State::default();
    for (id, owner, parent) in [
        ("parent", "parent", None),
        ("borrower", "parent", Some("parent")),
        ("owned", "owned", Some("parent")),
        ("depth-two", "owned", Some("owned")),
    ] {
        let record = crate::controller::test_support::checkpoint_test_session(id);
        assert_eq!(record.state, SessionState::Running);
        state.sessions.insert(id.into(), record);
        state.environment_owners.insert(id.into(), owner.into());
        if let Some(parent) = parent {
            state.subagents.insert(
                id.into(),
                SubagentRecord {
                    child_session_id: id.into(),
                    parent_session_id: parent.into(),
                    task_name: id.into(),
                    profile_id: "codex".into(),
                    working_directory: Default::default(),
                    request_key: id.into(),
                    created_at: "2026-06-01T00:00:00Z".into(),
                    kind: SubagentKind::Worker,
                    reviewer_launch: None,
                    model: None,
                    effort: None,
                    initial_prompt: "inspect".into(),
                    noticed_turn: None,
                    reported_finish: None,
                    handback_tool: true,
                    orchestrator: owner == id,
                    resume_with_parent: false,
                }
                .into(),
            );
        }
    }
    let children = super::move_children(&state, "parent");
    assert_eq!(
        children
            .iter()
            .map(|child| child.child_session_id.as_str())
            .collect::<Vec<_>>(),
        ["borrower"]
    );
    let refusal = super::roleless_move_children_error(&state, "parent").unwrap();
    assert!(
        refusal.contains("borrower")
            && !refusal.contains("owned")
            && !refusal.contains("depth-two")
    );
}

#[cfg(unix)]
mod staged_in_place_tests {
    use super::*;
    use crate::controller::checkpoint::stand_in::*;
    use crate::controller::move_session::test_fixtures::*;
    use mj_core::config::HarnessKind;
    use mj_worker::stand_in_relay::seed_source_relay;
    use std::fs;

    fn executor() -> RecordingProcessExecutor {
        RecordingProcessExecutor::new(WorkerCheckpoint {
            restore: mj_worker::checkpoint::restore_from_spec_file,
            export: |mut input| {
                Ok(serde_json::to_vec(
                    &mj_worker::checkpoint::export_from_spec_reader(&mut input)?,
                )?)
            },
        })
    }

    fn isolated(name: &str) -> bool {
        if std::env::var_os("MJ_TEST_STAGED_MOVE_CHILD").is_some() {
            return true;
        }
        let dir = tempfile::tempdir().unwrap();
        crate::controller::test_support::IsolatedTest::new(
            crate::controller::test_support::test_name(module_path!(), name),
        )
        .env("MJ_TEST_STAGED_MOVE_CHILD", "1")
        .env("MJ_TEST_RECORD_IN_PLACE_MOVE_REQUESTS", "1")
        .env(
            LATCH_RELAY_MAX_PROTOCOL,
            mj_core::relay::RELAY_PROTOCOL_VERSION.to_string(),
        )
        .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
        .isolated_store(dir.path())
        .run();
        false
    }

    fn live_files(root: &std::path::Path) -> Vec<Vec<u8>> {
        [
            "launch.json",
            "ownership.json",
            "profile/settings.json",
            "profile/source-only.txt",
        ]
        .map(|name| fs::read(root.join(name)).unwrap())
        .into()
    }

    fn assert_candidates_empty(root: &std::path::Path) {
        let path = root.join(mj_core::worker_launch::RELOAD_CANDIDATES_DIR);
        if path.exists() {
            assert_eq!(fs::read_dir(path).unwrap().count(), 0);
        }
    }

    #[test]
    fn in_place_move_precommit_refusal_keeps_live_files_and_clears_fence() {
        if isolated("in_place_move_precommit_refusal_keeps_live_files_and_clears_fence") {
            run_case("refused");
        }
    }

    #[test]
    fn in_place_move_staged_success_promotes_destination_and_removes_candidate() {
        if isolated("in_place_move_staged_success_promotes_destination_and_removes_candidate") {
            run_case("success");
        }
    }

    #[test]
    fn in_place_move_lost_reply_retry_skips_staging_and_restart() {
        if isolated("in_place_move_lost_reply_retry_skips_staging_and_restart") {
            run_case("lost-reply");
        }
    }

    #[test]
    fn in_place_move_failed_committed_reload_and_dead_recovery_settle_destination_error() {
        if isolated(
            "in_place_move_failed_committed_reload_and_dead_recovery_settle_destination_error",
        ) {
            run_case("failed");
        }
    }

    #[test]
    fn in_place_move_database_failure_after_commit_keeps_fence_until_recovery() {
        if !isolated("in_place_move_database_failure_after_commit_keeps_fence_until_recovery") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
        let relay_root = seed_source_relay(&fixture.worker_root);
        fs::write(relay_root.join("fail-in-place-after-commit"), "test").unwrap();
        let connection = rusqlite::Connection::open(crate::database::database_path()).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_destination_session BEFORE UPDATE ON sessions WHEN NEW.last_profile = 'claude-destination' BEGIN SELECT RAISE(ABORT, 'injected destination write failure'); END;").unwrap();
        let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let channels = start_source_relay_manager(&relay_root);
                let executor = executor();
                let admission =
                    crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
                let error = fixture
                    .controller
                    .execute_move(
                        &admission,
                        &mut operation,
                        None,
                        &executor,
                        &channels.control,
                        super::super::MoveSourceRelay::default(),
                    )
                    .await
                    .unwrap_err();
                channels.shutdown.shutdown().await.unwrap();
                assert!(
                    format!("{error:#}").contains("injected destination write failure"),
                    "{error:#}"
                );
                assert!(operation.in_place_expected_store_id.is_some());
                assert_eq!(
                    crate::database::load_session_record(LATCH_RELAY_SESSION)
                        .unwrap()
                        .unwrap()
                        .last_profile,
                    IN_PLACE_SOURCE_PROFILE
                );
                let receipt = mj_core::worker_launch::WorkerLaunchConfig::read(
                    &fixture.worker_root.join("launch.json"),
                )
                .unwrap();
                assert_eq!(
                    receipt.accepted_reload_operation_id.as_deref(),
                    Some(operation.operation_id.as_str())
                );
                let outcome = fixture
                    .controller
                    .finish_move_result(&mut operation, Err(error), &executor)
                    .unwrap();
                assert_eq!(outcome.outcome, "interrupted");
                assert!(
                    crate::database::load_move_operation(LATCH_RELAY_SESSION)
                        .unwrap()
                        .unwrap()
                        .in_place_expected_store_id
                        .is_some()
                );
                connection
                    .execute_batch("DROP TRIGGER reject_destination_session;")
                    .unwrap();
                fixture.controller.state = crate::database::load_state().unwrap();
                let channels = start_source_relay_manager(&relay_root);
                let outcome = fixture
                    .controller
                    .recover_move_managed_controlled(
                        &admission,
                        operation,
                        &executor,
                        &channels.control,
                    )
                    .await
                    .unwrap();
                channels.shutdown.shutdown().await.unwrap();
                assert_eq!(outcome.outcome, "failed", "{outcome:?}");
                let session = crate::database::load_session_record(LATCH_RELAY_SESSION)
                    .unwrap()
                    .unwrap();
                assert_eq!(session.last_profile, IN_PLACE_DESTINATION_PROFILE);
                assert_eq!(session.state, SessionState::Error);
                assert!(
                    crate::database::load_move_operation(LATCH_RELAY_SESSION)
                        .unwrap()
                        .unwrap()
                        .in_place_expected_store_id
                        .is_none()
                );
            });
    }

    #[test]
    fn in_place_move_cancelled_after_started_settles_destination_and_installs_recent_handoff() {
        if !isolated(
            "in_place_move_cancelled_after_started_settles_destination_and_installs_recent_handoff",
        ) {
            return;
        }
        use mj_core::relay::{
            RelayCommand, RelayCommandOutcome, RelayRequest, RelayRequestEnvelope,
            RelayResponseBody, RelayResponsePayload,
        };
        let _writer = crate::database::install_isolated_test_writer();
        let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Codex);
        let source = fixture.controller.state.sessions[LATCH_RELAY_SESSION].clone();
        let relay_root = seed_source_relay(&fixture.worker_root);
        let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
        let envelope = |request| RelayRequestEnvelope {
            request_id: "cancel-seed".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request,
        };
        let mut seed =
            mj_worker::relay::DurableRelay::open(&relay_root, LATCH_RELAY_SESSION, "1.0.0")
                .unwrap();
        let history =
            "Keep the source conversation context when cancellation retains destination ownership.";
        seed.handle(envelope(RelayRequest::Submit {
            command_id: "cmd-cancel-history".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                    history,
                )],
            },
        }));
        assert_eq!(seed.claim_pending_commands(true).unwrap().len(), 1);
        seed.handle(envelope(RelayRequest::Submit {
            command_id: "cmd-existing-user".into(),
            command: RelayCommand::Prompt {
                prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                    "existing queued user work",
                )],
            },
        }));
        let store = seed.operational_state().store_id.unwrap();
        let held = seed.handle(envelope(RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: operation.operation_id.clone(),
            expected_store_id: store.clone(),
        }));
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::InPlaceMoveDispatchHeld {
                    captured_pending_command_ids,
                },
        } = held.body
        else {
            panic!("hold expected")
        };
        assert_eq!(captured_pending_command_ids, ["cmd-existing-user"]);
        seed.record_command_completed(
            "cmd-cancel-history",
            RelayCommandOutcome::Prompt {
                work_facts: None,
                diagnostic: None,
                stop_reason: "end_turn".into(),
                usage: None,
            },
        )
        .unwrap();
        drop(seed);
        operation.in_place_expected_store_id = Some(store.clone());
        operation.in_place_held_command_ids = captured_pending_command_ids;
        crate::database::save_move_operation(&operation).unwrap();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let executor = executor();
            let admission = crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
            let channels = start_source_relay_manager(&relay_root);
            let mut relay = super::super::MoveSourceRelay::lease(&admission, &channels.control, LATCH_RELAY_SESSION).await.unwrap();
            let owner = relay.owner().unwrap();
            fixture.controller.apply_in_place_destination(&operation).unwrap();
            let backend = mj_core::targets::TargetLocator::LocalBare { worker_root: fixture.worker_root.to_string_lossy().into_owned() };
            let candidate = mj_core::relay::new_command_id("cancel-candidate").unwrap();
            owner.scope_blocking(|| fixture.controller.prepare_worker_files_for(LATCH_RELAY_SESSION, &backend, fixture.worker_root.to_str().unwrap(), &executor, mj_worker_staging::worker_binary::WorkerFilePreparation::StageCandidate { operation_id: operation.operation_id.clone(), expected_store_id: store.clone(), candidate_id: candidate.clone() }, mj_worker_staging::worker_binary::LaunchHistory::Stored)).unwrap();
            let launch = fixture.controller.current_worker_launch_config(LATCH_RELAY_SESSION, &backend).unwrap();
            let conn = relay.0.as_mut().unwrap().connection_mut();
            conn.client.restart_harness_in_place(&operation.operation_id, &store, Box::new(launch), Some(candidate)).await.unwrap();
            let hold = conn.client.status().await.unwrap().in_place_move_hold.unwrap();
            assert!(matches!(hold.reload_phase, Some(mj_core::relay::InPlaceReloadPhase::Started)));
            // The reply was lost before the controller published destination B.
            fixture.controller.state.sessions.insert(LATCH_RELAY_SESSION.into(), source);
            assert_eq!(crate::database::load_session_record(LATCH_RELAY_SESSION).unwrap().unwrap().last_profile, IN_PLACE_SOURCE_PROFILE);
            drop(relay);
            channels.shutdown.shutdown().await.unwrap();
            operation.cancellation_requested = true;
            operation.queue = ResumeQueueDisposition::Start;
            operation.quota_stop_reason = Some("quota exhausted".into());
            operation.quota_continuation = Some(mj_core::state::QuotaMoveContinuation::Prompt);
            operation.in_place_continuation_command_id = Some("cmd-must-not-continue".into());
            crate::database::save_move_operation(&operation).unwrap();
            let channels = start_source_relay_manager(&relay_root);
            let outcome = fixture.controller.recover_move_managed_controlled(&admission, operation, &executor, &channels.control).await.unwrap();
            channels.shutdown.shutdown().await.unwrap();
            assert_eq!(outcome.outcome, "cancelled", "{outcome:?}");
            assert!(outcome.error.as_deref().unwrap().contains("destination retained"));
            let record = crate::database::load_session_record(LATCH_RELAY_SESSION).unwrap().unwrap();
            assert_eq!(record.last_profile, IN_PLACE_DESTINATION_PROFILE);
            assert_eq!(record.harness_kind, HarnessKind::Claude);
            let operation = crate::database::load_move_operation(LATCH_RELAY_SESSION).unwrap().unwrap();
            assert!(operation.in_place_harness_restarted);
            assert!(operation.in_place_expected_store_id.is_none());
            assert!(operation.in_place_held_command_ids.is_empty());
            let snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(&fs::read(relay_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap()).unwrap();
            assert!(snapshot.in_place_move_hold.is_none());
            assert!(!snapshot.handled_commands.contains_key("cmd-must-not-continue"));
            assert_eq!(snapshot.dispatches["cmd-existing-user"].state, mj_core::relay::RelayDispatchState::InFlight);
            let context = snapshot.pending_prompt_context.unwrap();
            assert!(context.text.contains(history), "{}", context.text);
            assert_eq!(context.attached_command_id.as_deref(), Some("cmd-existing-user"));
            let requests = fs::read_to_string(relay_root.join("in-place-move-requests.log")).unwrap();
            assert!(!requests.contains("submit_front"));
            assert!(!requests.contains("discard_held_move_commands"));
        });
    }

    fn run_case(mode: &str) {
        let _writer = crate::database::install_isolated_test_writer();
        let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
        let source_session = fixture.controller.state.sessions[LATCH_RELAY_SESSION].clone();
        let relay_root = seed_source_relay(&fixture.worker_root);
        let before = live_files(&fixture.worker_root);
        let marker = match mode {
            "refused" => Some("reject-in-place-restart"),
            "lost-reply" => Some("lose-in-place-restart-reply"),
            "failed" => Some("fail-in-place-after-commit"),
            _ => None,
        };
        if let Some(marker) = marker {
            fs::write(relay_root.join(marker), "test").unwrap();
        }
        let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let channels = start_source_relay_manager(&relay_root);
                let executor = executor();
                let admission =
                    crate::controller::test_support::session_admission(LATCH_RELAY_SESSION);
                let result = fixture
                    .controller
                    .execute_move(
                        &admission,
                        &mut operation,
                        None,
                        &executor,
                        &channels.control,
                        super::super::MoveSourceRelay::default(),
                    )
                    .await;
                channels.shutdown.shutdown().await.unwrap();
                match mode {
                    "refused" => {
                        assert!(result.is_err());
                        assert_eq!(live_files(&fixture.worker_root), before);
                        assert_candidates_empty(&fixture.worker_root);
                        assert!(operation.in_place_expected_store_id.is_none());
                        assert!(operation.in_place_held_command_ids.is_empty());
                        let session = crate::database::load_session_record(LATCH_RELAY_SESSION)
                            .unwrap()
                            .unwrap();
                        assert_eq!(session.last_profile, IN_PLACE_SOURCE_PROFILE);
                        assert_eq!(session.state, SessionState::Running);
                        let relay = mj_worker::relay::DurableRelay::open(
                            &relay_root,
                            LATCH_RELAY_SESSION,
                            "1.0.0",
                        )
                        .unwrap();
                        assert!(relay.operational_state().in_place_move_hold.is_none());
                    }
                    "success" => {
                        result.unwrap();
                        let session = crate::database::load_session_record(LATCH_RELAY_SESSION)
                            .unwrap()
                            .unwrap();
                        assert_eq!(session.last_profile, IN_PLACE_DESTINATION_PROFILE);
                        assert_eq!(session.state, SessionState::Running);
                        assert_candidates_empty(&fixture.worker_root);
                        let launch = mj_core::worker_launch::WorkerLaunchConfig::read(
                            &fixture.worker_root.join("launch.json"),
                        )
                        .unwrap();
                        assert_eq!(
                            launch.accepted_reload_operation_id.as_deref(),
                            Some(operation.operation_id.as_str())
                        );
                        assert!(
                            fs::read_to_string(fixture.worker_root.join("profile/settings.json"))
                                .unwrap()
                                .contains("destination")
                        );
                    }
                    "lost-reply" => {
                        assert!(result.is_err());
                        assert!(operation.in_place_expected_store_id.is_some());
                        assert_eq!(
                            crate::database::load_session_record(LATCH_RELAY_SESSION)
                                .unwrap()
                                .unwrap()
                                .last_profile,
                            IN_PLACE_SOURCE_PROFILE
                        );
                        fs::write(
                            fixture.worker_root.join("profile/settings.json"),
                            "worker-configured destination",
                        )
                        .unwrap();
                        fs::write(
                            fixture.destination_home.join("settings.json"),
                            "invalid canonical settings: staging must be skipped",
                        )
                        .unwrap();
                        let channels = start_source_relay_manager(&relay_root);
                        let outcome = fixture
                            .controller
                            .recover_move_managed_controlled(
                                &admission,
                                operation.clone(),
                                &executor,
                                &channels.control,
                            )
                            .await
                            .unwrap();
                        channels.shutdown.shutdown().await.unwrap();
                        assert_eq!(outcome.outcome, "completed", "{outcome:?}");
                        assert_eq!(
                            fs::read_to_string(fixture.worker_root.join("profile/settings.json"))
                                .unwrap(),
                            "worker-configured destination"
                        );
                        assert_eq!(
                            crate::database::load_session_record(LATCH_RELAY_SESSION)
                                .unwrap()
                                .unwrap()
                                .last_profile,
                            IN_PLACE_DESTINATION_PROFILE
                        );
                        assert_eq!(
                            fs::read_to_string(relay_root.join("in-place-move-requests.log"))
                                .unwrap()
                                .matches("restart_harness_in_place")
                                .count(),
                            1
                        );
                        assert_candidates_empty(&fixture.worker_root);
                    }
                    "failed" => {
                        let error = result.unwrap_err();
                        fixture
                            .controller
                            .finish_move_result(&mut operation, Err(error), &executor)
                            .unwrap();
                        let session = crate::database::load_session_record(LATCH_RELAY_SESSION)
                            .unwrap()
                            .unwrap();
                        assert_eq!(session.last_profile, IN_PLACE_DESTINATION_PROFILE);
                        assert_eq!(session.state, SessionState::Error);
                        assert_eq!(
                            session.last_error.as_deref(),
                            Some("replacement harness failed after commit")
                        );
                        assert!(operation.in_place_expected_store_id.is_none());
                        assert_eq!(operation.phase, MovePhase::Failed);
                        let committed = live_files(&fixture.worker_root);
                        // Recreate the daemon crash window: worker committed B and
                        // wrote its exit failure, while the database still names A.
                        crate::database::save_session(&source_session).unwrap();
                        fixture
                            .controller
                            .state
                            .sessions
                            .insert(LATCH_RELAY_SESSION.into(), source_session);
                        operation.phase = MovePhase::Capturing;
                        operation.error = None;
                        operation.in_place_expected_store_id =
                            mj_worker::relay::DurableRelay::open(
                                &relay_root,
                                LATCH_RELAY_SESSION,
                                "1.0.0",
                            )
                            .unwrap()
                            .operational_state()
                            .store_id;
                        crate::database::save_move_operation(&operation).unwrap();
                        let channels = start_source_relay_manager(&relay_root);
                        let outcome = fixture
                            .controller
                            .recover_move_managed_controlled(
                                &admission,
                                operation.clone(),
                                &executor,
                                &channels.control,
                            )
                            .await
                            .unwrap();
                        channels.shutdown.shutdown().await.unwrap();
                        assert_eq!(outcome.outcome, "failed", "{outcome:?}");
                        let session = crate::database::load_session_record(LATCH_RELAY_SESSION)
                            .unwrap()
                            .unwrap();
                        assert_eq!(session.last_profile, IN_PLACE_DESTINATION_PROFILE);
                        assert_eq!(session.state, SessionState::Error);
                        assert_eq!(
                            session.last_error.as_deref(),
                            Some("replacement harness failed after commit")
                        );
                        assert!(
                            crate::database::load_move_operation(LATCH_RELAY_SESSION)
                                .unwrap()
                                .unwrap()
                                .in_place_expected_store_id
                                .is_none()
                        );
                        assert_eq!(live_files(&fixture.worker_root), committed);
                    }
                    _ => unreachable!(),
                }
            });
    }
}

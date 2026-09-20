use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, TextContent};
use anyhow::Result;

use super::{Controller, MoveMutationGuard, move_owns_session, move_refuses_command};
use crate::controller::test_support::{
    IsolatedTest, RefusingExecutor, checkpoint_test_session, committed_repository, local_bundle,
    managed_raw_session, raw_session_on, resume_compatibility_config, ssh_worktree_target,
};
#[cfg(unix)]
use mj_checkpoint::archive::{
    ArchiveInput, BundleManifest, CanonicalExecutionState, CanonicalQueuedCommandKind,
    CanonicalQueuedPrompt, CanonicalSessionSnapshot, CanonicalSessionState, SessionManifest,
    TargetManifest, write_archive_atomic,
};
use mj_core::config::{Config, HarnessKind, HarnessProfile};
#[cfg(unix)]
use mj_core::state::{
    CheckpointMetadata, MoveOperation, MovePhase, MoveSelection, ResumeQueueDisposition,
    TargetLocator,
};
#[cfg(unix)]
use mj_core::test_hooks::install_fake_command;

use mj_core::state::{
    MaterializedExecutionState, MaterializedQueuedPrompt, MaterializedSession, QueuedCommandKind,
    SessionResourceAllocation, SessionState, State,
};

use crate::targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};
use mj_core::relay::{RelayCommand, RelayRequest, read_relay_frame, write_relay_frame};
use mj_worker::relay::DurableRelay;

const PREPARE_PREFLIGHT_CHILD: &str = "MJ_MOVE_PREPARE_PREFLIGHT_CHILD";
const PREPARATION_SNAPSHOT_CHILD: &str = "MJ_MOVE_PREPARATION_SNAPSHOT_CHILD";
#[cfg(unix)]
const RECOVERY_TERMINAL_CHILD: &str = "MJ_MOVE_RECOVERY_TERMINAL_CHILD";
const MOVE_QUEUE_RELAY_ROOT: &str = "MJ_MOVE_QUEUE_RELAY_ROOT";
const MOVE_QUEUE_RELAY_MARKER: &str = "MJ_MOVE_QUEUE_RELAY_MARKER";
const MOVE_QUEUE_SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

fn add_codex_profile(config: &mut Config, home: &Path) {
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: home.to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
}

fn isolated_test_child(test_name: &str, marker: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(test_name)
        .env(marker, "1")
        .isolated_store(directory.path())
        .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
        .run();
    false
}

fn test_name(short: &str) -> String {
    crate::controller::test_support::test_name(module_path!(), short)
}

#[cfg(unix)]
fn shell_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// The reconnect plan for a local-bare worker executes `<worker_root>/hel`.
/// Install a tiny executable there so this test exercises the real
/// `StandaloneSession` protocol and `DurableRelay`, including process EOF.
#[cfg(unix)]
fn install_move_queue_relay_worker(root: &Path, marker: &Path, marker_exists: bool) {
    fs::create_dir_all(root).unwrap();
    if marker_exists {
        fs::write(marker, b"already served").unwrap();
    }
    let script = format!(
        "#!/bin/sh\nexport {root_var}={root}\nexport {marker_var}={marker}\n{binary} --exact {child} --nocapture | grep --line-buffered '^{{'\n",
        root_var = MOVE_QUEUE_RELAY_ROOT,
        marker_var = MOVE_QUEUE_RELAY_MARKER,
        root = shell_literal(root),
        marker = shell_literal(marker),
        binary = shell_literal(&std::env::current_exe().unwrap()),
        child = test_name("move_queue_relay_child"),
    );
    install_fake_command(root, "hel", &script);
}

/// Serve one durable relay connection. The first queue submission is ACKed
/// and then the process exits, modelling a crash after acceptance but before
/// the controller can persist queue admission. A reconnect reopens the same
/// store and serves every request normally; duplicate IDs are handled by the
/// relay's durable ledger.
#[test]
fn move_queue_relay_child() {
    let Some(root) = std::env::var_os(MOVE_QUEUE_RELAY_ROOT) else {
        return;
    };
    println!();
    let root = PathBuf::from(root);
    let marker = PathBuf::from(
        std::env::var_os(MOVE_QUEUE_RELAY_MARKER).expect("relay crash marker is configured"),
    );
    let crash_after_first = !marker.exists();
    let mut relay = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "1.0.0")
        .expect("open move queue test relay");
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    while let Some(request) = read_relay_frame(&mut input).expect("read relay request") {
        if let RelayRequest::Submit {
            command_id,
            command,
        } = &request.request
        {
            let observed = marker.with_extension("observed");
            let bytes = serde_json::to_vec(command).expect("serialize observed relay command");
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(observed)
                .expect("open relay observation log");
            writeln!(log, "{command_id} {}", bytes.len()).expect("record relay command");
        }
        let should_crash = crash_after_first
            && matches!(
                &request.request,
                RelayRequest::Submit {
                    command: RelayCommand::Prompt { .. } | RelayCommand::SetConfig { .. },
                    ..
                }
            );
        let response = relay.handle(request);
        write_relay_frame(&mut output, &response).expect("write relay response");
        if should_crash {
            fs::write(&marker, b"accepted then crashed").expect("record relay crash");
            return;
        }
    }
}

#[test]
fn mutation_guard_rejects_conflicting_commands_but_allows_observation() {
    let session_id = "move-guard-test";
    let guard = MoveMutationGuard::reserve(session_id).unwrap();
    assert!(move_owns_session(session_id));
    assert!(MoveMutationGuard::reserve(session_id).is_err());
    assert!(move_refuses_command(
        session_id,
        &RelayCommand::Prompt { prompt: Vec::new() }
    ));
    assert!(move_refuses_command(
        session_id,
        &RelayCommand::SetConfig {
            key: "model".into(),
            value: "test".into(),
        }
    ));
    assert!(!move_refuses_command(
        session_id,
        &RelayCommand::RecordNotice {
            text: "read-only notice".into(),
        }
    ));
    drop(guard);
    assert!(!move_owns_session(session_id));
    assert!(!move_refuses_command(
        session_id,
        &RelayCommand::Prompt { prompt: Vec::new() }
    ));
}

#[test]
fn move_configuration_rejects_a_destination_that_would_drop_a_pin() {
    let accepted = mj_core::acp::AcceptedSessionConfig {
        model: Some("opus".into()),
        effort: Some("xhigh".into()),
    };
    let choices = mj_core::worker_launch::ProfileConfig {
        model: Some("opus[1m]".into()),
        models: vec![mj_core::acp::SessionConfigChoice {
            value: "opus[1m]".into(),
            name: "Opus (1M context)".into(),
            description: None,
        }],
        efforts: vec![mj_core::acp::SessionConfigChoice {
            value: "xhigh".into(),
            name: "Extra high".into(),
            description: None,
        }],
        observed_at: 1,
    };

    let error = super::validate_preserved_configuration("claude3", &accepted, &choices)
        .unwrap_err()
        .to_string();

    assert!(error.contains("does not offer the session's accepted model \"opus\""));
    assert!(error.contains("opus[1m]"));
}

#[cfg(unix)]
#[test]
fn terminal_move_recovery_finishes_interrupted_close_before_phase_retry() {
    let short = "terminal_move_recovery_finishes_interrupted_close_before_phase_retry";
    if !isolated_test_child(&test_name(short), RECOVERY_TERMINAL_CHILD) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    for (prefix, phase, cancellation_requested, expected_outcome) in [
        ('1', MovePhase::Failed, false, "failed"),
        ('2', MovePhase::Cancelled, true, "cancelled"),
    ] {
        let session_id = format!("{prefix}{}", &MOVE_QUEUE_SESSION_ID[1..]);
        let directory = tempfile::tempdir().unwrap();
        let worker_root = directory.path().join(&session_id);
        fs::create_dir_all(&worker_root).unwrap();
        let checkpoint = crate::controller::test_support::write_checkpoint_gate_archive(
            directory.path(),
            &session_id,
            7,
        );
        let mut session = checkpoint_test_session(&session_id);
        session.state = SessionState::Destroying;
        session.target_template_id = "local-bare".into();
        session.target = Some(TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        });
        session.checkpoint = Some(checkpoint.clone());
        let state = State {
            sessions: BTreeMap::from([(session_id.clone(), session.clone())]),
            ..State::default()
        };
        crate::database::save_state(&state).unwrap();

        let mut controller = Controller {
            config: config.clone(),
            state,
        };
        let selection = MoveSelection {
            clear_resource_allocation: false,
            session_id: session_id.clone(),
            profile_id: Some("codex".into()),
            target_template_id: Some("local-bare".into()),
            additional_mounts: None,
            resource_allocation: None,
        };
        let operation = MoveOperation {
            in_place: false,
            source_checkpoint_only: false,
            operation_id: format!("move-recovery-terminal-{prefix}"),
            selection,
            source_profile_id: "codex".into(),
            source_target_template_id: "local-bare".into(),
            source_target: session.target.clone(),
            source_native_session_id: session.native_session_id.clone(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: "terminal-recovery-test".into(),
            checkpoint: Some(checkpoint),
            recovery_session: Some(session),
            queue: ResumeQueueDisposition::Discard,
            phase,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested,
            created_at: "2026-08-14T12:00:00Z".into(),
            updated_at: "2026-08-14T12:00:00Z".into(),
            error: None,
        };
        let manager = runtime
            .block_on(async { crate::session_manager::spawn_session_manager() })
            .unwrap();
        let outcome = runtime
            .block_on(controller.recover_move_managed_controlled(
                operation,
                &ProcessExecutor,
                &manager.control,
            ))
            .unwrap();
        runtime.block_on(manager.shutdown.shutdown()).unwrap();

        assert_eq!(outcome.outcome, expected_outcome);
        assert_eq!(
            outcome.error.as_deref(),
            Some(
                "Move source stop recovered; no destination work was started. Retry Move or Resume with previous settings"
            )
        );
        let recovered = &controller.state.sessions[&session_id];
        assert_eq!(recovered.state, SessionState::Stopped);
        assert!(recovered.target.is_none());
        assert!(!worker_root.exists());
    }
}

#[test]
fn move_configuration_fingerprint_changes_when_destination_changes() {
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    let session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            ..State::default()
        },
    };
    let selection = mj_core::state::MoveSelection {
        clear_resource_allocation: false,
        session_id: "0123456789abcdef0123456789abcdef".into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("podman".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let original = controller
        .move_configuration_fingerprint(&selection)
        .unwrap();
    let mut changed = controller.config.clone();
    let mj_core::config::TargetTemplate::LocalPodman { container } =
        changed.targets.get_mut("podman").unwrap()
    else {
        panic!("fixture target is not Podman");
    };
    container.image = "changed.example.invalid/image:latest".into();
    let changed_controller = Controller {
        config: changed,
        state: controller.state.clone(),
    };
    assert_ne!(
        original,
        changed_controller
            .move_configuration_fingerprint(&selection)
            .unwrap()
    );
}

#[test]
fn move_preflight_rejects_invalid_destination_before_source_mutation() {
    let short = "move_preflight_rejects_invalid_destination_before_source_mutation";
    if !isolated_test_child(&test_name(short), PREPARE_PREFLIGHT_CHILD) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    let session_id = "0123456789abcdef0123456789abcdef";

    let mut running = checkpoint_test_session(session_id);
    running.state = SessionState::Running;
    let previous = running.clone();
    let state = State {
        sessions: BTreeMap::from([(session_id.into(), running)]),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();

    let cases = [
        (
            "unknown target",
            mj_core::state::MoveSelection {
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("missing-target".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            "unknown destination target",
        ),
        (
            "unknown profile",
            mj_core::state::MoveSelection {
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("missing-profile".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            "unknown destination profile",
        ),
        (
            "invalid resource allocation",
            mj_core::state::MoveSelection {
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: Some(SessionResourceAllocation::Container {
                    cpus: 0,
                    memory_bytes: 0,
                }),
            },
            "resource allocation",
        ),
    ];

    for (label, selection, expected) in cases {
        let controller = Controller {
            config: config.clone(),
            state: state.clone(),
        };
        let error =
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(controller.prepare_move_session_controlled(
                    selection,
                    &RefusingExecutor("move preflight"),
                ))
                .unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains(expected), "{label}: {detail}");
        assert_eq!(controller.state.sessions[session_id], previous, "{label}");
        assert!(
            crate::database::load_move_operation(session_id)
                .unwrap()
                .is_none(),
            "{label} wrote move intent before preflight completed"
        );
    }

    let mut incompatible = managed_raw_session(ssh_worktree_target());
    incompatible.state = SessionState::Running;
    incompatible.bundle_id = "project".into();
    let incompatible_state = State {
        sessions: BTreeMap::from([(session_id.into(), incompatible.clone())]),
        ..State::default()
    };
    crate::database::save_state(&incompatible_state).unwrap();
    let controller = Controller {
        config,
        state: incompatible_state.clone(),
    };
    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(controller.prepare_move_session_controlled(
            mj_core::state::MoveSelection {
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            &RefusingExecutor("move preflight"),
        ))
        .unwrap_err();
    assert!(format!("{error:#}").contains("resume it on a bare target there"));
    assert_eq!(controller.state.sessions[session_id], incompatible);
    assert!(
        crate::database::load_move_operation(session_id)
            .unwrap()
            .is_none()
    );
}

fn queued_prompt(command_id: &str) -> MaterializedQueuedPrompt {
    MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: command_id.into(),
        kind: QueuedCommandKind::Prompt,
        content: Vec::new(),
        queued_at_ms: 1,
    }
}

#[cfg(unix)]
fn move_queue_checkpoint(directory: &Path, session_id: &str) -> CheckpointMetadata {
    let archive_path = directory.join("move-queue.hel.zip");
    let image = ContentBlock::Image(ImageContent::new("x".repeat(70 * 1024), "image/png"));
    let canonical = CanonicalSessionSnapshot {
        event_frontier: 0,
        event_frontier_digest: mj_checkpoint::archive::EVENT_FRONTIER_GENESIS_DIGEST.into(),
        session: CanonicalSessionState {
            execution: CanonicalExecutionState::Idle,
            last_activity_at_ms: None,
            session_title: None,
            configuration: BTreeMap::new(),
        },
        transcript: Vec::new(),
        queued_prompts: vec![
            CanonicalQueuedPrompt {
                command_id: "queued-image".into(),
                kind: CanonicalQueuedCommandKind::Prompt,
                content: vec![serde_json::to_value(image).unwrap()],
                queued_at_ms: 1,
            },
            CanonicalQueuedPrompt {
                command_id: "queued-config".into(),
                kind: CanonicalQueuedCommandKind::SetConfig {
                    key: "model".into(),
                    value: "large-test-model".into(),
                },
                content: vec![
                    serde_json::to_value(ContentBlock::Text(TextContent::new(
                        "/model large-test-model",
                    )))
                    .unwrap(),
                ],
                queued_at_ms: 2,
            },
        ],
    };
    let verified = write_archive_atomic(
        &archive_path,
        &ArchiveInput {
            session: SessionManifest {
                id: session_id.into(),
                title: "move queue replay".into(),
                harness_kind: HarnessKind::Codex,
                profile_id: "codex".into(),
                native_session_id: "native-session".into(),
                created_at: "2026-08-12T00:00:00Z".into(),
                checkpointed_at: "2026-08-14T12:00:00Z".into(),
                hel_version: "test".into(),
                relay_version: "test".into(),
                adapter_version: "test".into(),
            },
            target: TargetManifest {
                template_id: "local-bare".into(),
                target_kind: "local-bare".into(),
                details: BTreeMap::new(),
            },
            bundle: BundleManifest {
                id: "project".into(),
                primary_repository: "project".into(),
            },
            canonical_session: canonical,
            native_artifacts: Vec::new(),
            repositories: Vec::new(),
        },
    )
    .unwrap();
    CheckpointMetadata {
        archive_path,
        sha256: verified.archive_sha256,
        created_at: "2026-08-14T12:00:00Z".into(),
        event_frontier: 0,
    }
}

#[cfg(unix)]
#[test]
fn move_queue_replay_survives_accept_then_relay_crash_and_rejects_replaced_store() {
    let short = "move_queue_replay_survives_accept_then_relay_crash_and_rejects_replaced_store";
    if !isolated_test_child(&test_name(short), "MJ_MOVE_QUEUE_REPLAY_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let worker_root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    let marker = directory.path().join("first-relay.marker");
    install_move_queue_relay_worker(&worker_root, &marker, false);

    let replacement_directory = tempfile::tempdir().unwrap();
    let replacement_root = replacement_directory.path().join(MOVE_QUEUE_SESSION_ID);
    let replacement_marker = replacement_directory.path().join("replacement.marker");
    install_move_queue_relay_worker(&replacement_root, &replacement_marker, true);

    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    let target = TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    };
    let mut session = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    // This relay belongs to a raw checkout, not an unconfigured bundle.
    session.project_directory = Some(directory.path().to_path_buf());
    session.target_template_id = "local-bare".into();
    session.target = Some(target.clone());
    session.native_session_id = None;
    session.state = SessionState::Running;
    let state = State {
        sessions: BTreeMap::from([(session.id.clone(), session)]),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    crate::database::save_materialized_session(&MaterializedSession::empty(MOVE_QUEUE_SESSION_ID))
        .unwrap();

    let mut controller = Controller { config, state };
    let selection = MoveSelection {
        clear_resource_allocation: false,
        session_id: MOVE_QUEUE_SESSION_ID.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("local-bare".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let checkpoint = move_queue_checkpoint(directory.path(), MOVE_QUEUE_SESSION_ID);
    let fingerprint = controller
        .move_configuration_fingerprint(&selection)
        .unwrap();
    let mut operation = MoveOperation {
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-queue-replay".into(),
        selection,
        source_profile_id: "codex".into(),
        source_target_template_id: "local-bare".into(),
        source_target: None,
        source_native_session_id: None,
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: Some(target),
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: fingerprint,
        checkpoint: Some(checkpoint),
        recovery_session: None,
        queue: ResumeQueueDisposition::Start,
        phase: MovePhase::StartingQueue,
        queue_admission_started: true,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-08-14T12:00:00Z".into(),
        updated_at: "2026-08-14T12:00:00Z".into(),
        error: None,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    super::restore_move_queue_hold(&operation);
    let first = runtime
        .block_on(controller.admit_move_queue(&mut operation, &RefusingExecutor("move preflight")));
    assert!(first.is_err(), "the first relay must crash after its ACK");
    assert!(move_refuses_command(
        MOVE_QUEUE_SESSION_ID,
        &RelayCommand::Cancel
    ));
    // The durable admission hold survives an attempt's transient owner.
    drop(MoveMutationGuard::reserve(MOVE_QUEUE_SESSION_ID).unwrap());
    assert!(move_owns_session(MOVE_QUEUE_SESSION_ID));
    let destination_store_id = operation
        .destination_store_id
        .clone()
        .expect("store identity is persisted before queue replay");
    assert!(!operation.queue_admission_finished);

    runtime
        .block_on(controller.admit_move_queue(&mut operation, &RefusingExecutor("move preflight")))
        .unwrap();
    assert!(operation.queue_admission_finished);
    assert!(!move_owns_session(MOVE_QUEUE_SESSION_ID));
    let observed = fs::read_to_string(marker.with_extension("observed")).unwrap();
    assert!(observed.contains("queued-image"));
    assert!(observed.contains("queued-config"));
    let image_line = observed
        .lines()
        .find(|line| line.starts_with("queued-image "))
        .unwrap();
    let image_bytes: usize = image_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        image_bytes > 64 * 1024,
        "large image crossed the relay protocol"
    );
    let reopened = DurableRelay::open(&worker_root, MOVE_QUEUE_SESSION_ID, "1.0.0").unwrap();
    let operational = reopened.operational_state();
    assert_eq!(
        operational
            .active_prompt
            .as_ref()
            .map(|prompt| prompt.command_id.as_str()),
        Some("queued-image")
    );
    assert_eq!(
        operational
            .queued_prompts
            .iter()
            .map(|queued| queued.command_id.as_str())
            .collect::<Vec<_>>(),
        ["queued-config"]
    );

    let replacement_target = TargetLocator::LocalBare {
        worker_root: replacement_root,
    };
    controller
        .state
        .sessions
        .get_mut(MOVE_QUEUE_SESSION_ID)
        .expect("move queue session exists")
        .target = Some(replacement_target.clone());
    operation.destination_target = Some(replacement_target);
    let error = runtime
        .block_on(controller.admit_move_queue(&mut operation, &RefusingExecutor("move preflight")))
        .unwrap_err();
    assert!(format!("{error:#}").contains("storage was replaced"));
    assert_eq!(
        operation.destination_store_id.as_deref(),
        Some(destination_store_id.as_str())
    );
}

#[test]
fn move_preparation_captures_active_and_queue_changes_in_a_new_fingerprint() {
    let short = "move_preparation_captures_active_and_queue_changes_in_a_new_fingerprint";
    if !isolated_test_child(&test_name(short), PREPARATION_SNAPSHOT_CHILD) {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let repository = committed_repository();
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));

    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = raw_session_on("local-bare", &repository.path().to_string_lossy());
    session.bundle_id = "project".into();
    session.state = SessionState::Running;
    let state = State {
        sessions: BTreeMap::from([(session_id.into(), session.clone())]),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    let mut materialized = MaterializedSession::empty(session_id);
    materialized.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    materialized.queued_prompts.push(queued_prompt("queued-1"));
    crate::database::save_materialized_session(&materialized).unwrap();

    let controller = Controller {
        config: config.clone(),
        state: state.clone(),
    };
    let selection = mj_core::state::MoveSelection {
        clear_resource_allocation: false,
        session_id: session_id.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("local-bare".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let first = runtime
        .block_on(controller.prepare_move_session_controlled(selection.clone(), &ProcessExecutor))
        .unwrap();
    assert!(first.active);
    assert_eq!(
        first
            .queued_commands
            .iter()
            .map(|command| command.command_id.as_str())
            .collect::<Vec<_>>(),
        ["queued-1"]
    );

    materialized.execution = MaterializedExecutionState::Idle;
    materialized.queued_prompts = vec![queued_prompt("queued-2")];
    crate::database::save_materialized_session(&materialized).unwrap();
    let second = runtime
        .block_on(controller.prepare_move_session_controlled(selection, &ProcessExecutor))
        .unwrap();
    assert!(!second.active);
    assert_eq!(
        second
            .queued_commands
            .iter()
            .map(|command| command.command_id.as_str())
            .collect::<Vec<_>>(),
        ["queued-2"]
    );
    assert_ne!(first.fingerprint, second.fingerprint);
}

#[cfg(unix)]
#[test]
fn move_preparation_can_refresh_a_dead_source_without_starting_its_harness() {
    let name = test_name("move_preparation_can_refresh_a_dead_source_without_starting_its_harness");
    if !isolated_test_child(&name, "MJ_MOVE_DEAD_SOURCE_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let session = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    crate::database::save_session(&session).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let manager = crate::session_manager::spawn_session_manager().unwrap();
        manager
            .targets
            .send(vec![crate::session_manager::RelaySessionTarget {
                session_id: MOVE_QUEUE_SESSION_ID.into(),
                spec: CommandSpec::new("sh", ["-c", "exit 1"]),
                worker_recovery: None,
                project_memory: None,
            }])
            .unwrap();
        let snapshot = super::refresh_move_source(&manager.control, MOVE_QUEUE_SESSION_ID)
            .await
            .unwrap();
        assert!(snapshot.is_none());
        manager.shutdown.shutdown().await.unwrap();
    });
}

#[test]
fn in_place_eligibility_requires_same_target_mounts_and_allocation() {
    let mut source = raw_session_on("local-bare", "/home/dev/project");
    source.state = SessionState::Running;
    source.target = Some(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/tmp/worker"),
    });
    source.additional_mounts = Vec::new();
    source.resource_allocation = None;
    let baseline = mj_core::state::MoveSelection {
        clear_resource_allocation: false,
        session_id: source.id.clone(),
        profile_id: Some("codex".into()),
        target_template_id: Some("local-bare".into()),
        additional_mounts: Some(Vec::new()),
        resource_allocation: None,
    };

    let another_allocation = Some(SessionResourceAllocation::Container {
        cpus: 4,
        memory_bytes: 8 * 1024 * 1024 * 1024,
    });
    let another_mount = Some(vec![crate::targets::AdditionalMount {
        source: PathBuf::from("/home/dev/notes"),
        destination: PathBuf::from("/mnt/notes"),
        access: crate::targets::MountAccess::Ro,
    }]);

    // Each row is one departure from the baseline, and the flag says whether
    // the environment can be kept.
    let cases: Vec<(&str, mj_core::state::MoveSelection, bool, bool, bool)> = vec![
        ("profile only", baseline.clone(), false, false, true),
        (
            "different target template",
            mj_core::state::MoveSelection {
                target_template_id: Some("ssh-bare".into()),
                ..baseline.clone()
            },
            false,
            false,
            false,
        ),
        (
            "different attached mounts",
            mj_core::state::MoveSelection {
                additional_mounts: another_mount,
                ..baseline.clone()
            },
            false,
            false,
            false,
        ),
        (
            "different resource allocation",
            mj_core::state::MoveSelection {
                resource_allocation: another_allocation,
                ..baseline.clone()
            },
            false,
            false,
            false,
        ),
        (
            "cleared resource allocation",
            mj_core::state::MoveSelection {
                clear_resource_allocation: true,
                ..baseline.clone()
            },
            false,
            false,
            false,
        ),
        ("retry", baseline.clone(), false, true, false),
        ("sub-agent", baseline.clone(), true, false, false),
    ];
    for (label, selection, is_subagent, retry, expected) in cases {
        assert_eq!(
            super::in_place_move_eligible(&source, &selection, is_subagent, retry),
            expected,
            "{label}"
        );
    }

    // A source with no provisioned target has no environment to keep, and a
    // stopped source has already lost it.
    let mut without_target = source.clone();
    without_target.target = None;
    assert!(!super::in_place_move_eligible(
        &without_target,
        &baseline,
        false,
        false
    ));
    let mut stopped = source.clone();
    stopped.state = SessionState::Stopped;
    assert!(!super::in_place_move_eligible(
        &stopped, &baseline, false, false
    ));
    let mut disconnected = source;
    disconnected.state = SessionState::Disconnected;
    assert!(super::in_place_move_eligible(
        &disconnected,
        &baseline,
        false,
        false
    ));
}

#[cfg(unix)]
fn source_recovery_operation(session: &mj_core::state::SessionRecord) -> MoveOperation {
    MoveOperation {
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-source-recovery".into(),
        selection: MoveSelection {
            clear_resource_allocation: false,
            session_id: session.id.clone(),
            profile_id: Some("destination".into()),
            target_template_id: Some("local-bare".into()),
            additional_mounts: None,
            resource_allocation: None,
        },
        source_profile_id: session.last_profile.clone(),
        source_target_template_id: session.target_template_id.clone(),
        source_target: session.target.clone(),
        source_native_session_id: session.native_session_id.clone(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: "source-recovery-test".into(),
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Discard,
        phase: MovePhase::ClosingSource,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
        error: None,
    }
}

#[cfg(unix)]
#[test]
fn move_source_recovery_retains_data_on_cancellation_or_failed_stop_and_keeps_its_mode() {
    let name = test_name(
        "move_source_recovery_retains_data_on_cancellation_or_failed_stop_and_keeps_its_mode",
    );
    if !isolated_test_child(&name, "MJ_MOVE_SOURCE_STOP_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("retained-data"), b"source work").unwrap();
    let mut session = raw_session_on("local-bare", directory.path().to_str().unwrap());
    session.state = SessionState::Running;
    session.target = Some(TargetLocator::LocalBare {
        worker_root: root.clone(),
    });
    crate::database::save_session(&session).unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, directory.path());
    let mut controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(session.id.clone(), session.clone())]),
            ..State::default()
        },
    };
    struct StopFails {
        cancelled: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl CommandExecutor for StopFails {
        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("source process group is still running")
        }
        fn cancellation_requested(&self) -> bool {
            self.cancelled
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let manager = crate::session_manager::spawn_session_manager().unwrap();
        manager
            .targets
            .send(vec![crate::session_manager::RelaySessionTarget {
                session_id: session.id.clone(),
                spec: CommandSpec::new("sh", ["-c", "exit 1"]),
                worker_recovery: None,
                project_memory: None,
            }])
            .unwrap();
        // An in-place operation recovers exactly like a fresh-environment one:
        // the source keeps its data either way, and the maintenance mode it
        // records is the same.
        for (cancelled, in_place) in [(true, false), (false, false), (true, true), (false, true)] {
            let mut operation = source_recovery_operation(&session);
            operation.in_place = in_place;
            crate::database::save_move_operation(&operation).unwrap();
            let executor = StopFails {
                cancelled,
                calls: 0.into(),
            };
            let error = controller
                .prepare_move_source_checkpoint(
                    &session.id,
                    &executor,
                    &manager.control,
                    &mut operation,
                )
                .await
                .unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains(if cancelled {
                    "cancelled"
                } else {
                    "still running"
                }),
                "{message}"
            );
            assert_eq!(
                executor.calls.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!cancelled)
            );
            assert_eq!(
                fs::read(root.join("retained-data")).unwrap(),
                b"source work"
            );
            assert!(!root.join("hel.next").exists());
            assert!(!root.join("launch.json").exists());
            let persisted = crate::database::load_move_operation(&session.id)
                .unwrap()
                .unwrap();
            assert_eq!(persisted.source_checkpoint_only, !cancelled);
        }
        let (backend, _) = controller.worker_placement(&session.id).unwrap();
        assert_eq!(
            controller
                .current_worker_launch_config(&session.id, &backend)
                .unwrap()
                .run_mode,
            mj_core::worker_launch::WorkerRunMode::CheckpointOnly
        );
        // A fresh ordinary Resume must not inherit the source's maintenance mode.
        controller
            .state
            .sessions
            .get_mut(&session.id)
            .unwrap()
            .state = SessionState::Stopped;
        assert_eq!(
            controller
                .current_worker_launch_config(&session.id, &backend)
                .unwrap()
                .run_mode,
            mj_core::worker_launch::WorkerRunMode::Harness
        );
        manager.shutdown.shutdown().await.unwrap();
    });
}

/// Real Git for the checkout, canned answers for the container runtime, and
/// the fixture's network URL rewritten wherever Git really contacts a remote.
struct GitWithPodmanPreflightExecutor {
    remote: PathBuf,
}

impl CommandExecutor for GitWithPodmanPreflightExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if command.program == "git" {
            return crate::controller::test_support::FixtureRemoteExecutor {
                remote: self.remote.clone(),
            }
            .execute(command);
        }
        assert_eq!(command.program, "podman", "unexpected {}", command.program);
        let stdout: &[u8] = if command.args.iter().any(|argument| argument == "--version") {
            b"podman version 5.4.2\n"
        } else if command.args.iter().any(|argument| argument == "info") {
            b"true\n"
        } else {
            b"         0       1000          1\n         1     100000      65536\n"
        };
        Ok(CommandOutput {
            status: 0,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[test]
fn preparing_a_local_session_for_a_container_previews_the_conversion() {
    let short = "preparing_a_local_session_for_a_container_previews_the_conversion";
    if !isolated_test_child(&test_name(short), "MJ_MOVE_CONVERSION_PREVIEW_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let repository = committed_repository();
    let (_remote_parent, remote) =
        crate::controller::test_support::network_remote_for(repository.path());
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));

    let session_id = "0123456789abcdef0123456789abcdef";
    let mut session = raw_session_on("local-bare", &repository.path().to_string_lossy());
    session.bundle_id = "project".into();
    session.state = SessionState::Running;
    let state = State {
        sessions: BTreeMap::from([(session_id.into(), session)]),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    let controller = Controller { config, state };
    let selection = mj_core::state::MoveSelection {
        clear_resource_allocation: false,
        session_id: session_id.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("podman".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let executor = GitWithPodmanPreflightExecutor { remote };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let first = runtime
        .block_on(controller.prepare_move_session_controlled(selection.clone(), &executor))
        .unwrap();

    let preview = first.conversion.as_deref().expect("a conversion preview");
    assert_eq!(
        preview.fetch_url,
        crate::controller::test_support::FIXTURE_FETCH_URL
    );
    assert_eq!(preview.branch.as_deref(), Some("master"));
    assert_eq!(preview.default_branch, "master");
    // The move builds this session its first container, so the checkout lands
    // in the session's own workspace rather than the shared legacy one.
    assert_eq!(
        preview.destination,
        mj_core::targets::new_container_workspace(session_id)
            .unwrap()
            .join(repository.path().file_name().unwrap())
    );
    assert_eq!(preview.unpushed_commits, 0);
    assert_eq!(preview.untracked_files, 0);
    assert!(preview.host_checkout_retained);

    // A live agent keeps editing, so the dirty counts must not be able to
    // invalidate a confirmation the person is still reading.
    fs::write(repository.path().join("agent-edit.txt"), "written\n").unwrap();
    let second = runtime
        .block_on(controller.prepare_move_session_controlled(selection, &executor))
        .unwrap();

    assert_eq!(
        second.conversion.as_deref().unwrap().untracked_files,
        1,
        "the preview reports the new file"
    );
    assert_eq!(first.fingerprint, second.fingerprint);
}

#[test]
fn a_move_preparation_shows_queued_images_as_placeholders_without_their_bytes() {
    let mut queued = vec![mj_core::state::MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "queued-1".into(),
        kind: mj_core::state::QueuedCommandKind::Prompt,
        content: vec![
            serde_json::json!({"type": "text", "text": "look at this"}),
            serde_json::json!({"type": "image", "mimeType": "image/png", "data": "secret-image-bytes"}),
        ],
        queued_at_ms: 1,
    }];

    super::replace_queued_images_with_placeholders(&mut queued);

    let content = serde_json::to_string(&queued[0].content).unwrap();
    assert!(content.contains("look at this"));
    assert!(content.contains("[Image attachment: image/png]"));
    assert!(!content.contains("secret-image-bytes"));
}

#[cfg(unix)]
const IN_PLACE_MOVE_CHILD: &str = "MJ_MOVE_IN_PLACE_CHILD";

/// Where the in-place tests keep the session that owns the live relay. The
/// relay fixture these tests borrow from the checkpoint suite journals for this
/// exact session id.
#[cfg(unix)]
use crate::controller::checkpoint::tests::{
    LATCH_CHECKPOINT_ONLY, LATCH_RELAY_ROOT, LATCH_RELAY_SESSION, ReleaseSupport,
    latch_relay_target,
};

/// A `CommandExecutor` that really runs what it is given and remembers the
/// purpose of every command. An in-place move is judged by what it did *not*
/// run, so the commands have to be real and the record complete.
#[cfg(unix)]
#[derive(Default)]
struct RecordingProcessExecutor {
    purposes: std::sync::Mutex<Vec<String>>,
    /// Every command line, so a test can prove which paths were never named.
    command_lines: std::sync::Mutex<Vec<String>>,
    notices: std::sync::Mutex<Vec<String>>,
    /// Purpose substring whose command fails instead of running.
    fail_purpose: Option<&'static str>,
}

#[cfg(unix)]
impl RecordingProcessExecutor {
    fn purposes(&self) -> Vec<String> {
        self.purposes.lock().unwrap().clone()
    }

    fn notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().clone()
    }

    fn command_lines(&self) -> Vec<String> {
        self.command_lines.lock().unwrap().clone()
    }

    fn record(&self, command: &CommandSpec) {
        self.purposes.lock().unwrap().push(command.purpose.clone());
        self.command_lines.lock().unwrap().push(format!(
            "{} {}",
            command.program,
            command.args.join(" ")
        ));
    }
}

#[cfg(unix)]
impl CommandExecutor for RecordingProcessExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.record(command);
        if let Some(failing) = self.fail_purpose
            && command.purpose.contains(failing)
        {
            anyhow::bail!("{} failed in this test", command.purpose);
        }
        ProcessExecutor.execute(command)
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        self.record(command);
        ProcessExecutor.execute_with_stdin(command, input)
    }

    fn notify_notice(&self, notice: &str) {
        self.notices.lock().unwrap().push(notice.to_owned());
    }
}

/// Install the stand-in worker binary for a LocalBare in-place move.
///
/// The controller installs whatever `MJ_WORKER_BINARY` names as
/// `<worker_root>/hel`, and the checked-in fake dispatcher reads its behaviour
/// from `<worker_root>/hel.script`. The in-place reset unlinks `hel` but not
/// `hel.script`, so one script serves the harness before and after the swap:
/// `worker proxy` becomes the relay server the controller reconnects to, and
/// `worker restore-checkpoint` leaves the relay seed a real restore would.
#[cfg(unix)]
fn install_in_place_worker_script(worker_root: &Path, frontier: u64, digest: &str) {
    let script = format!(
        r#"case "${{2:-}}" in
proxy)
    unset {checkpoint_only}
    {relay_root}={root}
    export {relay_root}
    {binary} --exact {child} --nocapture | grep --line-buffered '^{{'
    ;;
restore-checkpoint)
    cat >{root}/relay-seed.json <<'SEED'
{{"event_frontier":{frontier},"event_frontier_digest":"{digest}","queued_prompts":[]}}
SEED
    ;;
esac
exit 0
"#,
        checkpoint_only = LATCH_CHECKPOINT_ONLY,
        relay_root = LATCH_RELAY_ROOT,
        root = shell_literal(worker_root),
        binary = shell_literal(&std::env::current_exe().unwrap()),
        child = "controller::checkpoint::tests::latch_relay_child_serves_stdio",
    );
    install_fake_command(worker_root, "hel", &script);
}

/// The state an in-place move starts its destination half from: the source
/// sealed, its checkpoint installed, and its target still on the record. That
/// is exactly what `an_in_place_move_close_seals_the_source_and_keeps_its_target`
/// proves a retained close leaves behind.
#[cfg(unix)]
struct InPlaceFixture {
    _directory: tempfile::TempDir,
    worker_root: PathBuf,
    checkout: PathBuf,
    source_home: PathBuf,
    destination_home: PathBuf,
    checkpoint: CheckpointMetadata,
    controller: Controller,
}

#[cfg(unix)]
const IN_PLACE_SOURCE_PROFILE: &str = "claude-source";
#[cfg(unix)]
const IN_PLACE_DESTINATION_PROFILE: &str = "claude-destination";

#[cfg(unix)]
fn in_place_fixture(destination_kind: HarnessKind, source_kind: HarnessKind) -> InPlaceFixture {
    let directory = tempfile::tempdir().unwrap();
    // A LocalBare worker root has to end in the session id.
    let worker_root = directory.path().join(LATCH_RELAY_SESSION);
    let checkout = directory.path().join("checkout");
    let archives = directory.path().join("archives");
    let source_home = directory.path().join("source-profile");
    let destination_home = directory.path().join("destination-profile");
    for path in [
        &worker_root,
        &checkout,
        &archives,
        &source_home,
        &destination_home,
    ] {
        fs::create_dir_all(path).unwrap();
    }
    // `stage_profile` copies an allowlist per harness kind, so the file that
    // tells the two profiles apart has to be one of those names.
    for (home, owner) in [(&source_home, "source"), (&destination_home, "destination")] {
        fs::write(
            home.join("settings.json"),
            format!(r#"{{"profile":"{owner}"}}"#),
        )
        .unwrap();
        fs::write(
            home.join("auth.json"),
            format!(r#"{{"profile":"{owner}"}}"#),
        )
        .unwrap();
    }
    // Work the swap must not touch: an untracked file beside the project and
    // anything else the session left in its worker root.
    fs::write(checkout.join("untracked.txt"), b"agent work").unwrap();
    fs::write(worker_root.join("build-cache.txt"), b"warm cache").unwrap();
    // What the sealed source left installed in the worker root.
    fs::create_dir_all(worker_root.join("profile")).unwrap();
    fs::write(
        worker_root.join("profile").join("settings.json"),
        br#"{"profile":"source"}"#,
    )
    .unwrap();
    // Nothing copies this file and nothing overwrites it: only removing the
    // previous profile home can make it disappear.
    fs::write(
        worker_root.join("profile").join("source-only.txt"),
        b"old harness state",
    )
    .unwrap();
    fs::write(worker_root.join("launch.json"), b"{}").unwrap();
    fs::write(
        worker_root.join("ownership.json"),
        serde_json::to_vec(&serde_json::json!({"profile_id": IN_PLACE_SOURCE_PROFILE})).unwrap(),
    )
    .unwrap();
    fs::write(worker_root.join(mj_core::relay::RELAY_STATE_FILE), b"{}").unwrap();
    fs::create_dir_all(worker_root.join(mj_core::relay::RELAY_JOURNAL_DIR)).unwrap();

    let checkpoint = crate::controller::test_support::write_checkpoint_gate_archive_for_harness(
        &archives,
        LATCH_RELAY_SESSION,
        2,
        source_kind,
        IN_PLACE_SOURCE_PROFILE,
    );
    install_in_place_worker_script(&worker_root, checkpoint.event_frontier, &"a".repeat(64));

    // `validate_move_checkpoint` re-reads the configuration from disk, so the
    // controller's configuration has to be the persisted one.
    let (config, ()) = Config::update(|config| {
        for (id, kind, home) in [
            (IN_PLACE_SOURCE_PROFILE, source_kind, &source_home),
            (
                IN_PLACE_DESTINATION_PROFILE,
                destination_kind,
                &destination_home,
            ),
        ] {
            config.profiles.insert(
                id.into(),
                HarnessProfile {
                    enabled: true,
                    kind,
                    home: home.clone(),
                    environment: BTreeMap::new(),
                    context_window_bytes: None,
                    guardian_review_model: None,
                },
            );
        }
        config.targets.insert(
            "local-bare".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        Ok(())
    })
    .unwrap();

    let mut session = checkpoint_test_session(LATCH_RELAY_SESSION);
    session.harness_kind = source_kind;
    session.last_profile = IN_PLACE_SOURCE_PROFILE.into();
    session.target_template_id = "local-bare".into();
    session.target = Some(TargetLocator::LocalBare {
        worker_root: worker_root.clone(),
    });
    session.project_directory = Some(checkout.clone());
    session.state = SessionState::Running;
    session.checkpoint = Some(checkpoint.clone());
    crate::database::save_session(&session).unwrap();
    crate::database::save_materialized_session(&MaterializedSession::empty(LATCH_RELAY_SESSION))
        .unwrap();

    let controller = Controller {
        config,
        state: State {
            sessions: BTreeMap::from([(LATCH_RELAY_SESSION.into(), session)]),
            ..State::default()
        },
    };
    InPlaceFixture {
        _directory: directory,
        worker_root,
        checkout,
        source_home,
        destination_home,
        checkpoint,
        controller,
    }
}

#[cfg(unix)]
fn in_place_operation(controller: &Controller, destination_profile: &str) -> MoveOperation {
    let selection = MoveSelection {
        clear_resource_allocation: false,
        session_id: LATCH_RELAY_SESSION.into(),
        profile_id: Some(destination_profile.into()),
        target_template_id: Some("local-bare".into()),
        additional_mounts: Some(Vec::new()),
        resource_allocation: None,
    };
    let source = &controller.state.sessions[LATCH_RELAY_SESSION];
    MoveOperation {
        in_place: true,
        source_checkpoint_only: false,
        operation_id: "move-in-place".into(),
        configuration_fingerprint: controller
            .move_configuration_fingerprint(&selection)
            .unwrap(),
        selection,
        source_profile_id: source.last_profile.clone(),
        source_target_template_id: source.target_template_id.clone(),
        source_target: source.target.clone(),
        source_native_session_id: source.native_session_id.clone(),
        source_additional_mounts: Vec::new(),
        source_resource_allocation: None,
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        checkpoint: None,
        recovery_session: None,
        queue: ResumeQueueDisposition::Discard,
        phase: MovePhase::Preparing,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: false,
        created_at: "2026-09-17T12:00:00Z".into(),
        updated_at: "2026-09-17T12:00:00Z".into(),
        error: None,
    }
}

/// Run one whole move through `execute_move`, with a live source relay the
/// session manager owns.
///
/// The source relay journals outside the worker root: the worker root is the
/// environment under test, and this stands in for the daemon the controller
/// talks to while it seals the source.
#[cfg(unix)]
fn run_in_place_move(
    controller: &mut Controller,
    operation: &mut MoveOperation,
    worker_root: &Path,
    executor: &RecordingProcessExecutor,
) -> Result<()> {
    let source_relay = seed_source_relay(worker_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let result = controller
            .execute_move(operation, None, executor, &channels.control)
            .await;
        channels.shutdown.shutdown().await.unwrap();
        result
    })
}

/// The source relay journal an in-place move starts from.
#[cfg(unix)]
fn seed_source_relay(worker_root: &Path) -> PathBuf {
    let source_relay = worker_root.parent().unwrap().join("source-relay");
    fs::create_dir_all(&source_relay).unwrap();
    let mut seed = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
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
    source_relay
}

/// A session manager serving the source relay. One of these is one daemon
/// lifetime, so a test models a restart by shutting one down and starting the
/// next against the same journal.
#[cfg(unix)]
fn start_source_relay_manager(
    source_relay: &Path,
) -> crate::session_manager::SessionManagerChannels {
    let channels = crate::session_manager::spawn_session_manager().unwrap();
    channels
        .targets
        .send(vec![latch_relay_target(
            source_relay,
            None,
            ReleaseSupport::Supported,
            false,
        )])
        .unwrap();
    channels
}

/// A profile-only move on an unchanged target replaces the harness inside the
/// environment it found: nothing is destroyed or created, the worker root and
/// the workspace survive, and only the previous profile home is removed.
#[cfg(unix)]
#[test]
fn in_place_move_reinstalls_the_harness_without_removing_the_worker_root() {
    let name = test_name("in_place_move_reinstalls_the_harness_without_removing_the_worker_root");
    if std::env::var_os(IN_PLACE_MOVE_CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env(IN_PLACE_MOVE_CHILD, "1")
            // The source relay advances its own Close, so the sealed source
            // needs no ACP harness behind it.
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let InPlaceFixture {
        _directory,
        worker_root,
        checkout,
        destination_home,
        checkpoint,
        mut controller,
        ..
    } = fixture;
    let source_target = controller.state.sessions[LATCH_RELAY_SESSION]
        .target
        .clone();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    crate::database::save_move_operation(&operation).unwrap();

    let executor = RecordingProcessExecutor::default();
    run_in_place_move(&mut controller, &mut operation, &worker_root, &executor).unwrap();

    let purposes = executor.purposes();
    for purpose in &purposes {
        let lowered = purpose.to_lowercase();
        assert!(
            !lowered.contains("remove exact")
                && !lowered.contains("podman")
                && !lowered.contains("docker")
                && !lowered.contains("create the session worker root")
                && !lowered.contains("clean"),
            "an in-place move must not rebuild anything, but it ran {purpose:?} \
             (all: {purposes:?})"
        );
    }
    assert_eq!(
        purposes
            .iter()
            .filter(
                |purpose| *purpose == "reset the worker root for an in-place harness replacement"
            )
            .count(),
        1,
        "exactly one reset: {purposes:?}"
    );
    assert!(
        !executor
            .notices()
            .iter()
            .any(|notice| notice == "Cleaning up source"),
        "an in-place move has no source storage to clean up: {:?}",
        executor.notices()
    );

    // The environment survived, the old harness did not.
    assert_eq!(
        fs::read(worker_root.join("build-cache.txt")).unwrap(),
        b"warm cache"
    );
    assert_eq!(
        fs::read(checkout.join("untracked.txt")).unwrap(),
        b"agent work"
    );
    // A Claude session only owns a staged profile home where CLAUDE_CONFIG_DIR
    // can point it at one. On macOS it reads Claude's own home instead, so the
    // worker root has no per-session profile for the move to replace.
    if cfg!(target_os = "macos") {
        assert!(
            worker_root.join("profile").join("source-only.txt").exists(),
            "a session that owns no staged profile home must not have one removed"
        );
    } else {
        assert!(
            !worker_root.join("profile").join("source-only.txt").exists(),
            "the previous profile home must be gone"
        );
        assert_eq!(
            fs::read(worker_root.join("profile").join("settings.json")).unwrap(),
            br#"{"profile":"destination"}"#,
            "the destination profile must be staged in its place"
        );
    }
    assert!(destination_home.join("settings.json").is_file());
    let ownership: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_root.join("ownership.json")).unwrap()).unwrap();
    assert_eq!(
        ownership["profile_id"].as_str(),
        Some(IN_PLACE_DESTINATION_PROFILE)
    );

    let persisted = crate::database::load_move_operation(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert!(persisted.in_place);
    assert_eq!(persisted.destination_target, source_target);
    assert_eq!(persisted.checkpoint.as_ref(), Some(&checkpoint));
    let moved = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(moved.state, SessionState::Running);
    assert_eq!(moved.last_profile, IN_PLACE_DESTINATION_PROFILE);
    assert_eq!(moved.target, source_target);
    // Same harness, so the archived native session carries the conversation.
    assert_eq!(moved.native_session_id.as_deref(), Some("native-session"));

    // The conversation says what happened, in the destination's own journal.
    let journal = fs::read_to_string(
        worker_root
            .join(mj_core::relay::RELAY_JOURNAL_DIR)
            .join(mj_core::relay::RELAY_ACTIVE_SEGMENT),
    )
    .unwrap();
    assert!(
        journal.contains("in place; the workspace and environment were kept"),
        "the move notice is missing from the destination journal: {journal}"
    );
}

/// The path of the checked-in fake command dispatcher, which these tests hand
/// the controller as the worker binary to install.
#[cfg(unix)]
fn fake_worker_dispatcher() -> PathBuf {
    mj_core::test_hooks::fake_command_dispatcher()
}

/// A profile whose home is the user's own directory has nothing the session
/// owns inside the target, so an in-place swap must not delete it.
#[cfg(unix)]
#[test]
fn in_place_move_never_removes_a_shared_local_profile_home() {
    let name = test_name("in_place_move_never_removes_a_shared_local_profile_home");
    if std::env::var_os("MJ_MOVE_IN_PLACE_SHARED_HOME_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_IN_PLACE_SHARED_HOME_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    // Codex without a custom model provider reads and writes the user's own
    // profile home on a LocalBare target.
    let InPlaceFixture {
        _directory,
        worker_root,
        source_home,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Codex, HarnessKind::Codex);
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    crate::database::save_move_operation(&operation).unwrap();

    let executor = RecordingProcessExecutor::default();
    run_in_place_move(&mut controller, &mut operation, &worker_root, &executor).unwrap();

    let home = source_home.to_string_lossy().into_owned();
    for command in executor.command_lines() {
        assert!(
            !(command.contains("rm ") && command.contains(&home)),
            "no command may remove the shared profile home: {command:?}"
        );
    }
    assert!(
        source_home.join("auth.json").is_file(),
        "the shared profile home survives the swap"
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
}

/// A cross-harness move keeps the environment too, and the new harness starts
/// with the handoff installed as its first prompt context. Nothing is
/// provisioned: no clone, no container, no worker root creation.
#[cfg(unix)]
#[test]
fn in_place_cross_harness_move_installs_the_handoff_without_provisioning() {
    let name = test_name("in_place_cross_harness_move_installs_the_handoff_without_provisioning");
    if std::env::var_os("MJ_MOVE_IN_PLACE_CROSS_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_IN_PLACE_CROSS_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    // The source archived a Claude session; the destination is Codex, so the
    // native session cannot carry over and a handoff has to.
    let InPlaceFixture {
        _directory,
        worker_root,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Codex, HarnessKind::Claude);
    let source_target = controller.state.sessions[LATCH_RELAY_SESSION]
        .target
        .clone();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    crate::database::save_move_operation(&operation).unwrap();

    let executor = RecordingProcessExecutor::default();
    run_in_place_move(&mut controller, &mut operation, &worker_root, &executor).unwrap();

    let purposes = executor.purposes();
    for purpose in &purposes {
        let lowered = purpose.to_lowercase();
        assert!(
            !lowered.contains("clone")
                && !lowered.contains("podman")
                && !lowered.contains("create the session worker root"),
            "a cross-harness in-place move provisions nothing, but it ran {purpose:?} \
             (all: {purposes:?})"
        );
    }
    // No utility model is configured in this isolated store, so the handoff is
    // the verbatim recent conversation. Either way it is installed as the new
    // harness's pending prompt context before the session reports Running.
    let snapshot: serde_json::Value = serde_json::from_slice(
        &fs::read(worker_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
    )
    .unwrap();
    let context = snapshot["pending_prompt_context"]["text"]
        .as_str()
        .expect("the destination relay holds the installed handoff");
    assert!(
        context.contains("shared transcript summary"),
        "unexpected handoff: {context}"
    );
    let moved = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(moved.state, SessionState::Running);
    assert_eq!(moved.harness_kind, HarnessKind::Codex);
    assert_eq!(moved.target, source_target);
}

/// An in-place swap is never retried in place. A failure releases the
/// environment and leaves the session stopped with its verified checkpoint,
/// which is what an ordinary Resume or a retried Move starts from.
#[cfg(unix)]
#[test]
fn in_place_move_failure_tears_down_and_leaves_stopped_with_checkpoint() {
    let name = test_name("in_place_move_failure_tears_down_and_leaves_stopped_with_checkpoint");
    if std::env::var_os("MJ_MOVE_IN_PLACE_FAILURE_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_IN_PLACE_FAILURE_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let InPlaceFixture {
        _directory,
        worker_root,
        checkpoint,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    crate::database::save_move_operation(&operation).unwrap();

    let executor = RecordingProcessExecutor {
        fail_purpose: Some("start detached Mjolnir worker"),
        ..RecordingProcessExecutor::default()
    };
    let failure =
        run_in_place_move(&mut controller, &mut operation, &worker_root, &executor).unwrap_err();
    assert!(
        format!("{failure:#}").contains("start detached Mjolnir worker"),
        "{failure:#}"
    );
    let outcome = controller
        .finish_move_result(&mut operation, Err(failure), &executor)
        .unwrap();
    assert_eq!(outcome.outcome, "failed");
    assert_eq!(
        outcome.recovery.as_deref(),
        Some(
            "Session is stopped with a verified checkpoint. Retry move or Resume with previous settings."
        )
    );

    let rolled_back = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(rolled_back.state, SessionState::Stopped);
    assert_eq!(rolled_back.target, None);
    assert_eq!(rolled_back.checkpoint.as_ref(), Some(&checkpoint));
    // The record the swap replaced is back, harness and profile included.
    assert_eq!(rolled_back.last_profile, IN_PLACE_SOURCE_PROFILE);
    assert_eq!(rolled_back.harness_kind, HarnessKind::Claude);
    // The environment was released rather than left half-swapped.
    assert!(
        !worker_root.exists(),
        "the rollback tears the target down: {}",
        worker_root.display()
    );
    let persisted = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(persisted.state, SessionState::Stopped);
    assert!(persisted.checkpoint.is_some());
}

/// A daemon restart in the middle of the swap does not resume it: the record
/// is rolled back to the source identity, the environment is released, and the
/// session is stopped with its verified checkpoint.
#[cfg(unix)]
#[test]
fn in_place_move_recovery_after_restart_during_swap_rolls_back_to_stopped() {
    let name = test_name("in_place_move_recovery_after_restart_during_swap_rolls_back_to_stopped");
    if std::env::var_os("MJ_MOVE_IN_PLACE_SWAP_RESTART_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_IN_PLACE_SWAP_RESTART_CHILD", "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let InPlaceFixture {
        _directory,
        worker_root,
        checkpoint,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let source = controller.state.sessions[LATCH_RELAY_SESSION].clone();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::ResumingDestination;
    operation.checkpoint = Some(checkpoint.clone());
    // What `restore_session_in_place` persisted before it was interrupted: the
    // destination identity on the record, the source identity in the operation.
    operation.recovery_session = Some(source.clone());
    crate::database::save_move_operation(&operation).unwrap();
    let interrupted = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    interrupted.state = SessionState::Provisioning;
    interrupted.harness_kind = HarnessKind::Claude;
    interrupted.last_profile = IN_PLACE_DESTINATION_PROFILE.into();
    crate::database::save_session(interrupted).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = runtime.block_on(async {
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let outcome = controller
            .recover_move_managed_controlled(operation, &ProcessExecutor, &channels.control)
            .await;
        channels.shutdown.shutdown().await.unwrap();
        outcome.unwrap()
    });

    assert_eq!(outcome.outcome, "failed");
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("destination restoration was interrupted")),
        "{:?}",
        outcome.error
    );
    let recovered = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(recovered.state, SessionState::Stopped);
    assert_eq!(recovered.target, None);
    assert_eq!(recovered.last_profile, IN_PLACE_SOURCE_PROFILE);
    assert_eq!(recovered.checkpoint.as_ref(), Some(&checkpoint));
    assert!(
        !worker_root.exists(),
        "the interrupted swap released its environment: {}",
        worker_root.display()
    );
}

/// A daemon restart before the swap started finishes the interrupted close
/// instead, and says that the environment the in-place move promised to keep
/// was released.
#[cfg(unix)]
#[test]
fn in_place_move_recovery_after_restart_before_swap_finishes_the_close() {
    let name = test_name("in_place_move_recovery_after_restart_before_swap_finishes_the_close");
    if std::env::var_os("MJ_MOVE_IN_PLACE_CLOSE_RESTART_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_IN_PLACE_CLOSE_RESTART_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let InPlaceFixture {
        _directory,
        worker_root,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::ClosingSource;
    crate::database::save_move_operation(&operation).unwrap();
    let source_relay = seed_source_relay(&worker_root);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = RecordingProcessExecutor::default();
    // One daemon lifetime: seal the source and keep its target, exactly as the
    // first half of an in-place move does.
    runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let deferred = controller
            .suspend_session_for_move(
                LATCH_RELAY_SESSION,
                &executor,
                &channels.control,
                &mut operation,
                None,
                crate::controller::lifecycle::SourceTargetDisposition::RetainForInPlaceSwap,
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        assert!(!deferred.unwrap());
    });
    let sealed = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(sealed.state, SessionState::Closing);
    assert!(sealed.target.is_some());

    // The next daemon lifetime recovers the interrupted move.
    let outcome = runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let outcome = controller
            .recover_move_managed_controlled(operation, &executor, &channels.control)
            .await;
        channels.shutdown.shutdown().await.unwrap();
        outcome.unwrap()
    });

    assert_eq!(outcome.outcome, "failed");
    assert_eq!(
        outcome.error.as_deref(),
        Some(
            "Move source stop recovered; no destination work was started. Retry Move or Resume with previous settings; the in-place swap was interrupted; the environment was released"
        )
    );
    let recovered = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(recovered.state, SessionState::Stopped);
    assert_eq!(recovered.target, None);
    assert!(recovered.checkpoint.is_some());
    assert!(
        !worker_root.exists(),
        "the recovered close tears the environment down: {}",
        worker_root.display()
    );
}

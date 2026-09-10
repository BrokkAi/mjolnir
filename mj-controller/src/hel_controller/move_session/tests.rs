use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, TextContent};
use anyhow::Result;

use super::{Controller, MoveMutationGuard, move_owns_session, move_refuses_command};
use crate::hel_controller::test_support::{
    checkpoint_test_session, committed_repository, local_bundle, managed_raw_session,
    raw_session_on, resume_compatibility_config, ssh_worktree_target,
};
#[cfg(unix)]
use hel::hel_archive::{
    ArchiveInput, BundleManifest, CanonicalExecutionState, CanonicalQueuedCommandKind,
    CanonicalQueuedPrompt, CanonicalSessionSnapshot, CanonicalSessionState, SessionManifest,
    TargetManifest, write_archive_atomic,
};
use hel::hel_config::{HarnessKind, HarnessProfile, HelConfig};
#[cfg(unix)]
use hel::hel_state::{
    CheckpointMetadata, MoveOperation, MovePhase, MoveSelection, ResumeQueueDisposition,
    TargetLocator,
};
use hel::hel_state::{
    HelState, MaterializedExecutionState, MaterializedQueuedPrompt, MaterializedSession,
    QueuedCommandKind, SessionResourceAllocation, SessionState,
};
use hel::hel_targets::{CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor};
use hel::hel_worker::{
    DurableRelay, RelayCommand, RelayRequest, read_relay_frame, write_relay_frame,
};

const PREPARE_PREFLIGHT_CHILD: &str = "MJ_MOVE_PREPARE_PREFLIGHT_CHILD";
const PREPARATION_SNAPSHOT_CHILD: &str = "MJ_MOVE_PREPARATION_SNAPSHOT_CHILD";
#[cfg(unix)]
const RECOVERY_TERMINAL_CHILD: &str = "MJ_MOVE_RECOVERY_TERMINAL_CHILD";
const MOVE_QUEUE_RELAY_ROOT: &str = "MJ_MOVE_QUEUE_RELAY_ROOT";
const MOVE_QUEUE_RELAY_MARKER: &str = "MJ_MOVE_QUEUE_RELAY_MARKER";
const MOVE_QUEUE_SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

struct UnusedExecutor;

impl CommandExecutor for UnusedExecutor {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        panic!("move preflight unexpectedly ran {}", command.program);
    }
}

fn add_codex_profile(config: &mut HelConfig, home: &Path) {
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            enabled: true,
            kind: HarnessKind::Codex,
            home: home.to_path_buf(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
        },
    );
}

fn isolated_test_child(test_name: &str, marker: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(marker, "1")
        .env("MJ_DATA_DIR", directory.path().join("data"))
        .env("MJ_CONFIG_DIR", directory.path().join("config"))
        .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated move test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn test_name(short: &str) -> String {
    format!(
        "{}::{short}",
        module_path!()
            .strip_prefix("mj_controller::")
            .unwrap_or(module_path!())
    )
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
    let binary = root.join("hel");
    fs::write(&binary, script).unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    fs::set_permissions(binary, permissions).unwrap();
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

#[cfg(unix)]
#[test]
fn terminal_move_recovery_finishes_interrupted_close_before_phase_retry() {
    let short = "terminal_move_recovery_finishes_interrupted_close_before_phase_retry";
    if !isolated_test_child(&test_name(short), RECOVERY_TERMINAL_CHILD) {
        return;
    }
    let _writer = hel::hel_database::install_isolated_test_writer();
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
        let checkpoint = crate::hel_controller::test_support::write_checkpoint_gate_archive(
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
        let state = HelState {
            sessions: BTreeMap::from([(session_id.clone(), session.clone())]),
            ..HelState::default()
        };
        hel::hel_database::save_state(&state).unwrap();

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
            .block_on(async { crate::hel_session_manager::spawn_session_manager() })
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
        state: HelState {
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            ..HelState::default()
        },
    };
    let selection = hel::hel_state::MoveSelection {
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
    let hel::hel_config::TargetTemplate::LocalPodman { container } =
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
    let _writer = hel::hel_database::install_isolated_test_writer();
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    let session_id = "0123456789abcdef0123456789abcdef";

    let mut running = checkpoint_test_session(session_id);
    running.state = SessionState::Running;
    let previous = running.clone();
    let state = HelState {
        sessions: BTreeMap::from([(session_id.into(), running)]),
        ..HelState::default()
    };
    hel::hel_database::save_state(&state).unwrap();

    let cases = [
        (
            "unknown target",
            hel::hel_state::MoveSelection {
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
            hel::hel_state::MoveSelection {
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
            hel::hel_state::MoveSelection {
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
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.prepare_move_session_controlled(selection, &UnusedExecutor))
            .unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains(expected), "{label}: {detail}");
        assert_eq!(controller.state.sessions[session_id], previous, "{label}");
        assert!(
            hel::hel_database::load_move_operation(session_id)
                .unwrap()
                .is_none(),
            "{label} wrote move intent before preflight completed"
        );
    }

    let mut incompatible = managed_raw_session(ssh_worktree_target());
    incompatible.state = SessionState::Running;
    incompatible.bundle_id = "project".into();
    let incompatible_state = HelState {
        sessions: BTreeMap::from([(session_id.into(), incompatible.clone())]),
        ..HelState::default()
    };
    hel::hel_database::save_state(&incompatible_state).unwrap();
    let controller = Controller {
        config,
        state: incompatible_state.clone(),
    };
    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(controller.prepare_move_session_controlled(
            hel::hel_state::MoveSelection {
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            &UnusedExecutor,
        ))
        .unwrap_err();
    assert!(format!("{error:#}").contains("resume it on a bare target there"));
    assert_eq!(controller.state.sessions[session_id], incompatible);
    assert!(
        hel::hel_database::load_move_operation(session_id)
            .unwrap()
            .is_none()
    );
}

fn queued_prompt(command_id: &str) -> MaterializedQueuedPrompt {
    MaterializedQueuedPrompt {
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
        event_frontier_digest: hel::hel_archive::EVENT_FRONTIER_GENESIS_DIGEST.into(),
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
    let _writer = hel::hel_database::install_isolated_test_writer();
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
    session.target_template_id = "local-bare".into();
    session.target = Some(target.clone());
    session.native_session_id = None;
    session.state = SessionState::Running;
    let state = HelState {
        sessions: BTreeMap::from([(session.id.clone(), session)]),
        ..HelState::default()
    };
    hel::hel_database::save_state(&state).unwrap();
    hel::hel_database::save_materialized_session(&MaterializedSession::empty(
        MOVE_QUEUE_SESSION_ID,
    ))
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
    let first = runtime.block_on(controller.admit_move_queue(&mut operation, &UnusedExecutor));
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
        .block_on(controller.admit_move_queue(&mut operation, &UnusedExecutor))
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
        .block_on(controller.admit_move_queue(&mut operation, &UnusedExecutor))
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
    let _writer = hel::hel_database::install_isolated_test_writer();
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
    let state = HelState {
        sessions: BTreeMap::from([(session_id.into(), session.clone())]),
        ..HelState::default()
    };
    hel::hel_database::save_state(&state).unwrap();
    let mut materialized = MaterializedSession::empty(session_id);
    materialized.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    materialized.queued_prompts.push(queued_prompt("queued-1"));
    hel::hel_database::save_materialized_session(&materialized).unwrap();

    let controller = Controller {
        config: config.clone(),
        state: state.clone(),
    };
    let selection = hel::hel_state::MoveSelection {
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
    hel::hel_database::save_materialized_session(&materialized).unwrap();
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

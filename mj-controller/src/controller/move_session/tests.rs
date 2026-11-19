#[cfg(unix)]
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use agent_client_protocol::v1::schema::{ContentBlock, ImageContent, TextContent};
use anyhow::Result;

#[cfg(unix)]
use super::MoveSourceRelay;
pub(crate) use super::test_fixtures::*;
use super::{Controller, MoveMutationGuard, move_owns_session, move_refuses_command};
use crate::controller::test_support::{
    IsolatedTest, RefusingExecutor, checkout_with_network_remote, checkpoint_test_session,
    committed_repository, local_bundle, managed_raw_session, raw_session_on,
    resume_compatibility_config, ssh_worktree_target,
};
#[cfg(unix)]
use mj_checkpoint::archive::{
    ArchiveInput, BundleManifest, CanonicalExecutionState, CanonicalQueuedCommandKind,
    CanonicalQueuedPrompt, CanonicalSessionSnapshot, CanonicalSessionState, SessionManifest,
    TargetManifest, write_archive_atomic,
};
use mj_core::config::{Config, HarnessKind, HarnessProfile, ProjectBundle, ProjectRepository};
#[cfg(unix)]
use mj_core::state::CheckpointMetadata;
use mj_core::state::{MoveOperation, MovePhase, MoveSelection, ResumeQueueDisposition};
#[cfg(unix)]
use mj_core::test_hooks::install_fake_command;
#[cfg(unix)]
use mj_worker::stand_in_relay::seed_source_relay;

use mj_core::state::{
    MaterializedExecutionState, MaterializedQueuedPrompt, MaterializedSession, QueuedCommandKind,
    SessionResourceAllocation, SessionState, State, TargetLocator,
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
const MOVE_QUEUE_GOAL: &str = "MJ_MOVE_QUEUE_GOAL";
fn add_codex_profile(config: &mut Config, home: &Path) {
    config.profiles.insert(
        "codex".into(),
        HarnessProfile {
            ..mj_core::config::HarnessProfile::new(HarnessKind::Codex, home.to_path_buf())
        },
    );
}

fn test_name(short: &str) -> String {
    crate::controller::test_support::test_name(module_path!(), short)
}

/// The reconnect plan for a local-bare worker executes `<worker_root>/hel`.
/// Install a tiny executable there so this test exercises the real
/// `StandaloneSession` protocol and `DurableRelay`, including process EOF.
#[cfg(unix)]
fn install_move_queue_relay_worker(
    root: &Path,
    marker: &Path,
    marker_exists: bool,
) -> TestProxyGroupGuard {
    fs::create_dir_all(root).unwrap();
    if marker_exists {
        fs::write(marker, b"already served").unwrap();
    }
    let guard = TestProxyGroupGuard::new(root);
    let script = format!(
        "#!/bin/sh\n{registration}export {root_var}={root}\nexport {marker_var}={marker}\n{binary} --exact {child} --nocapture | grep --line-buffered '^{{'\n",
        registration = guard.registration_script(),
        root_var = MOVE_QUEUE_RELAY_ROOT,
        marker_var = MOVE_QUEUE_RELAY_MARKER,
        root = shell_literal(root),
        marker = shell_literal(marker),
        binary = shell_literal(&std::env::current_exe().unwrap()),
        child = test_name("move_queue_relay_child"),
    );
    install_fake_command(root, "hel", &script);
    guard
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
    let wire_protocol = fs::read_to_string(root.join("protocol-version"))
        .ok()
        .map(|value| value.trim().parse::<u32>().unwrap())
        .unwrap_or(mj_core::relay::RELAY_PROTOCOL_VERSION);
    let crash_after_first = !marker.exists();
    // The delegation-only connection must not open a second journal writer.
    let mut relay = None;
    let mut baseline_download = None;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    while let Some(request) = read_relay_frame(&mut input).expect("read relay request") {
        use mj_core::relay::{RelayResponseBody, RelayResponseEnvelope, RelayResponsePayload};
        if matches!(request.request, RelayRequest::Status)
            && root.join("block-install-status").exists()
            && (!root.join("block-after-cut").exists() || root.join("baseline-frontier").exists())
        {
            fs::write(root.join("install-status-reached"), b"waiting").unwrap();
            while !root.join("release-install-status").exists() {
                if !root.join("block-install-status").exists() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let payload = match &request.request {
            RelayRequest::Hello { .. } => Some(RelayResponsePayload::Hello {
                negotiated: wire_protocol,
                relay_version: "1.0.0".into(),
                session_id: MOVE_QUEUE_SESSION_ID.into(),
                worker_build: None,
            }),
            RelayRequest::ReadAttachment { reference } => {
                use base64::Engine as _;
                let bytes = mj_core::attachment::AttachmentStore::worker(&root)
                    .read(reference)
                    .expect("read actual worker image blob");
                Some(RelayResponsePayload::AttachmentData {
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                })
            }
            RelayRequest::PollSubagentQueue { acknowledged } => {
                Some(RelayResponsePayload::SubagentQueue {
                    page: mj_core::subagent::SubagentQueuePage {
                        sequence: acknowledged.unwrap_or(0) + 1,
                        changes: Vec::new(),
                        has_more: false,
                    },
                })
            }
            // The queue never changes here: hold briefly, as the worker
            // would, and report nothing new.
            RelayRequest::WaitForChange {
                after_ordinal,
                state_digest,
            } => {
                std::thread::sleep(std::time::Duration::from_millis(200));
                Some(RelayResponsePayload::ChangeObserved {
                    latest_ordinal: *after_ordinal,
                    state_digest: state_digest.clone().unwrap_or_default(),
                    subagent_queue_changed: false,
                })
            }
            _ => None,
        };
        if let Some(payload) = payload {
            write_relay_frame(
                &mut output,
                &RelayResponseEnvelope {
                    request_id: request.request_id,
                    protocol_version: wire_protocol,
                    body: RelayResponseBody::Ok { payload },
                },
            )
            .expect("answer queue or hello");
            continue;
        }
        let relay = relay.get_or_insert_with(|| {
            let mut relay = if root.join("test-startup-commands.json").exists() {
                let initial = serde_json::from_slice::<Vec<mj_core::worker_launch::InitialWorkerCommand>>(
                    &fs::read(root.join("test-startup-commands.json")).unwrap()).unwrap();
                DurableRelay::open_for_harness(&root, MOVE_QUEUE_SESSION_ID, "1.0.0", HarnessKind::Codex, &initial)
            } else {
                DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "1.0.0")
            }.expect("open move queue test relay");
            let goal_mode = std::env::var(MOVE_QUEUE_GOAL).unwrap_or_default();
            if crash_after_first && matches!(goal_mode.as_str(), "1" | "active") {
                relay.record_session_update(serde_json::from_value(serde_json::json!({
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "mjGoalCapability": {"version": 1, "controlMethod": "_session/goal", "actions": ["resume", "pause"]},
                        "goal": {"objective": "Finish the interrupted work", "status": if goal_mode == "active" { "active" } else { "limited" }, "createdAt": 42,
                            "controlMethod": "_session/goal", "limitReason": "usage"},
                        "execution": {"version": 1, "status": "idle"}
                    }
                })).unwrap()).unwrap();
            }
            if crash_after_first && goal_mode == "active" {
                relay.record_observation(mj_core::relay::RelayObservation::SessionConfigured { config_options: vec![] }).unwrap();
                // Hard-won: #1498: quota-stopped active goals blocked automatic in-place Move.
                let operational = relay.operational_state();
                assert!(operational.safe_to_move(HarnessKind::Codex, mj_core::activity::MovePurpose::Manual));
                assert!(operational.safe_to_move(HarnessKind::Codex, mj_core::activity::MovePurpose::QuotaRetry), "{:?}", operational.facts());
            }
            relay
        });
        if matches!(
            request.request,
            RelayRequest::BeginLiveBaseline
                | RelayRequest::ReadLiveBaselinePage { .. }
                | RelayRequest::AbortLiveBaseline
        ) {
            let cut = matches!(request.request, RelayRequest::BeginLiveBaseline)
                .then(|| relay.live_session_baseline().unwrap());
            if let Some(cut) = &cut {
                fs::write(
                    root.join("baseline-frontier"),
                    cut.state.latest_ordinal.to_string(),
                )
                .unwrap();
                relay.record_session_update(serde_json::from_value(serde_json::json!({
                    "sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"after recovery cut"}
                })).unwrap()).unwrap();
                relay.record_observation(mj_core::relay::RelayObservation::NativeAgent { event: mj_core::native_agent::NativeAgentEvent::Update {
                    session_id: "staged-child".into(), update: Box::new(serde_json::from_value(serde_json::json!({
                        "sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"child after recovery cut"}
                    })).unwrap()),
                }}).unwrap();
                relay
                    .record_observation(mj_core::relay::RelayObservation::NativeAgent {
                        event: mj_core::native_agent::NativeAgentEvent::ReplayCommit,
                    })
                    .unwrap();
            }
            let payload = mj_core::relay::live_baseline_reply(
                &request.request,
                cut,
                &mut baseline_download,
                &request.request_id,
                request.protocol_version,
            )
            .unwrap();
            write_relay_frame(
                &mut output,
                &RelayResponseEnvelope {
                    request_id: request.request_id,
                    protocol_version: wire_protocol,
                    body: RelayResponseBody::Ok { payload },
                },
            )
            .unwrap();
            continue;
        }
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
                    command: RelayCommand::Prompt { .. }
                        | RelayCommand::SetConfig { .. }
                        | RelayCommand::GoalControl { .. },
                    ..
                }
            );
        assert!(
            request.request.supported_at(wire_protocol),
            "request exceeds emulated worker protocol"
        );
        let legacy_floor = match &request.request {
            RelayRequest::Acknowledge {
                through_ordinal,
                through_digest,
            } if wire_protocol < mj_core::relay::RELAY_COMMAND_RETENTION_PROTOCOL => {
                Some(mj_core::relay::RelayCursor {
                    ordinal: *through_ordinal,
                    digest: through_digest.clone(),
                })
            }
            _ => None,
        };
        let mut response = if matches!(request.request, RelayRequest::SetOperatorAway { .. }) {
            RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: wire_protocol,
                body: RelayResponseBody::Ok {
                    payload: RelayResponsePayload::Status(relay.operational_state()),
                },
            }
        } else {
            let mut request = request;
            request.protocol_version = mj_core::relay::RELAY_PROTOCOL_VERSION;
            relay.handle(request)
        };
        // The protocol71 fixture models a checkpoint floor alongside its ACK.
        if let Some(through) = legacy_floor {
            relay.handle(mj_core::relay::RelayRequestEnvelope {
                request_id: "legacy-floor".into(),
                protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
                request: RelayRequest::Submit {
                    command_id: format!("legacy-floor-{}", through.ordinal),
                    command: RelayCommand::AdvanceRecoveryFloor { through },
                },
            });
        }
        if root.join("test-native-ready").exists() {
            match &mut response.body {
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::Status(state),
                } => state.acp_ready = Some(true),
                RelayResponseBody::Ok {
                    payload:
                        RelayResponsePayload::Attached {
                            state,
                            state_digest,
                            ..
                        },
                } => {
                    state.acp_ready = Some(true);
                    *state_digest = Some(mj_core::relay::relay_state_digest(state).unwrap());
                }
                _ => {}
            }
        }
        if let Ok(kind) = fs::read_to_string(root.join("unusable-worker"))
            && let RelayResponseBody::Ok {
                payload: RelayResponsePayload::Status(state),
            } = &mut response.body
        {
            match kind.as_str() {
                "checkpoint-only" => state.checkpoint_only = true,
                "closed" => state.execution = mj_core::relay::RelayExecutionState::Closed,
                "closing" => state.execution = mj_core::relay::RelayExecutionState::Closing,
                _ => panic!("unknown unavailable worker state"),
            }
        }
        response.protocol_version = wire_protocol;
        write_relay_frame(&mut output, &response).expect("write relay response");
        if should_crash {
            if std::env::var(MOVE_QUEUE_GOAL).as_deref() == Ok("1") {
                // The accepted resume changes the live predicate before the
                // daemon can save queue completion or reconnect after the crash.
                relay.record_session_update(serde_json::from_value(serde_json::json!({
                    "sessionUpdate": "session_info_update",
                    "_meta": {
                        "goal": {"objective": "Finish the interrupted work", "status": "active", "createdAt": 42,
                            "controlMethod": "_session/goal"},
                        "execution": {"version": 1, "status": "running"}
                    }
                })).unwrap()).unwrap();
            }
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
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        }));
        session.checkpoint = Some(Box::new(checkpoint.clone()));
        let state = State {
            sessions: [(session_id.clone(), session.clone())]
                .into_iter()
                .collect(),
            ..State::default()
        };
        crate::database::save_state(&state).unwrap();

        let mut controller = Controller {
            config: config.clone(),
            state,
        };
        let selection = MoveSelection {
            oracle: None,
            oracle_choice: None,
            subagents: None,
            workspace: Default::default(),
            clear_resource_allocation: false,
            session_id: session_id.clone(),
            profile_id: Some("codex".into()),
            target_template_id: Some("local-bare".into()),
            additional_mounts: None,
            resource_allocation: None,
        };
        let operation = MoveOperation {
            quota_stop_reason: None,
            quota_continuation: None,
            prepared_destination: None,
            accepted_preparation: None,
            acknowledge_interruption: false,
            workspace_transfer: None,
            handoff: None,
            in_place: false,
            source_checkpoint_only: false,
            operation_id: format!("move-recovery-terminal-{prefix}"),
            selection,
            source_profile_id: "codex".into(),
            source_target_template_id: "local-bare".into(),
            source_target: session.target.clone().map(|v| *v),
            source_native_session_id: session.native_session_id.clone(),
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
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
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
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
            sessions: [(session.id.clone(), session)].into_iter().collect(),
            ..State::default()
        },
    };
    let selection = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
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
    config.profiles.insert(
        "kimi".into(),
        HarnessProfile {
            kind: HarnessKind::Kimi,
            ..config.profiles["codex"].clone()
        },
    );
    let session_id = "0123456789abcdef0123456789abcdef";

    let mut running = checkpoint_test_session(session_id);
    running.state = SessionState::Running;
    let previous = running.clone();
    let state = State {
        sessions: [(session_id.into(), running)].into_iter().collect(),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();

    let cases = [
        (
            "unknown target",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
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
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("missing-profile".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            "unknown profile \"missing-profile\"",
        ),
        (
            "invalid resource allocation",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
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
        (
            "delegation policy the destination harness cannot run",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                // None is accepted on every harness (#1284); Mjolnir still
                // needs the destination's delegation tools.
                subagents: Some(mj_core::subagent::SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                }),
                workspace: Default::default(),
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("kimi".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            "need a Claude, Codex, DSH, or OpenCode profile",
        ),
        (
            "unconfigured Mjolnir delegation",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: Some(mj_core::subagent::SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                }),
                workspace: Default::default(),
                clear_resource_allocation: false,
                session_id: session_id.into(),
                profile_id: Some("codex".into()),
                target_template_id: Some("podman".into()),
                additional_mounts: None,
                resource_allocation: None,
            },
            "[subagents.straightforward]",
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
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &RefusingExecutor("move preflight"),
            ))
            .unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains(expected), "{label}: {detail}");
        if label == "unconfigured Mjolnir delegation" {
            assert_eq!(
                mj_core::refusal::Refusal::of(&error).and_then(|r| r.code()),
                Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
            );
        }
        assert_eq!(controller.state.sessions[session_id], previous, "{label}");
        assert!(
            crate::database::load_move_operation(session_id)
                .unwrap()
                .is_none(),
            "{label} wrote move intent before preflight completed"
        );
    }

    // A Move's new delegation policy is written with the destination's
    // lifecycle fields.
    let mut moved = state.sessions[session_id].clone();
    moved.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    crate::database::save_resumed_session(&moved, None).unwrap();
    assert_eq!(
        crate::database::load_session_record(session_id)
            .unwrap()
            .unwrap()
            .subagents,
        Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None
        })
    );

    let mut incompatible = managed_raw_session(ssh_worktree_target());
    incompatible.state = SessionState::Running;
    incompatible.bundle_id = "project".into();
    let incompatible_state = State {
        sessions: [(session_id.into(), incompatible.clone())]
            .into_iter()
            .collect(),
        ..State::default()
    };
    crate::database::save_state(&incompatible_state).unwrap();
    let controller = Controller {
        config,
        state: incompatible_state.clone(),
    };
    let error =
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                mj_core::state::MoveSelection {
                    oracle: None,
                    oracle_choice: None,
                    subagents: None,
                    workspace: Default::default(),
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

/// Records what a Move tells the user about decisions it made for them.
struct NoticeExecutor(std::sync::Mutex<Vec<String>>);

impl CommandExecutor for NoticeExecutor {
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
        panic!("the Move unexpectedly ran {}", command.program);
    }

    fn notify_notice(&self, notice: &str) {
        self.0.lock().unwrap().push(notice.to_owned());
    }
}

#[test]
fn only_an_explicit_oracle_request_without_a_model_is_turned_off_with_a_notice() {
    let short = "only_an_explicit_oracle_request_without_a_model_is_turned_off_with_a_notice";
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
    running.oracle = false;
    let state = State {
        sessions: [(session_id.into(), running)].into_iter().collect(),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    let controller = Controller { config, state };
    let selection = |oracle, oracle_choice| mj_core::state::MoveSelection {
        oracle,
        oracle_choice,
        subagents: None,
        workspace: Default::default(),
        clear_resource_allocation: false,
        session_id: session_id.into(),
        profile_id: Some("codex".into()),
        // The Move stops at its unknown target, after the oracle decision.
        target_template_id: Some("missing".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let notices = |selection| {
        let executor = NoticeExecutor(Default::default());
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &executor,
            ))
            .unwrap_err();
        assert!(format!("{error:#}").contains("unknown destination target"));
        executor.0.into_inner().unwrap()
    };
    // Leaving the choice alone, and turning Oracle off, are never reported.
    assert!(notices(selection(None, None)).is_empty());
    assert!(notices(selection(Some(false), None)).is_empty());
    // Asking for it with no model anywhere is.
    assert_eq!(
        notices(selection(Some(true), None)),
        [mj_core::state::ORACLE_UNAVAILABLE_NOTICE]
    );
    let override_choice = mj_core::config::SubagentChoice {
        model: "advisor".into(),
        effort: None,
    };
    assert!(notices(selection(None, Some(override_choice))).is_empty());
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
        command_ledger: None,
        assessment_state: None,
        notification_stack: None,
        event_frontier: 0,
        event_frontier_digest: mj_checkpoint::archive::EVENT_FRONTIER_GENESIS_DIGEST.into(),
        session: CanonicalSessionState {
            execution: CanonicalExecutionState::Idle,
            last_activity_at_ms: None,
            session_title: None,
            configuration: Default::default(),
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
// Hard-won: #1498: quota-stopped active goals blocked automatic in-place Move.
#[test]
fn move_queue_replay_survives_accept_then_relay_crash_and_rejects_replaced_store() {
    let short = "move_queue_replay_survives_accept_then_relay_crash_and_rejects_replaced_store";
    if std::env::var_os("MJ_MOVE_QUEUE_REPLAY_CHILD").is_none() {
        for goal in ["0", "1", "active"] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(short))
                .env("MJ_MOVE_QUEUE_REPLAY_CHILD", "1")
                .env(MOVE_QUEUE_GOAL, goal)
                .isolated_store(directory.path())
                .env("MJ_WORKER_BINARY", std::env::current_exe().unwrap())
                .run();
        }
        return;
    }
    let goal = std::env::var(MOVE_QUEUE_GOAL).as_deref() == Ok("1");
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let worker_root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    let marker = directory.path().join("first-relay.marker");
    let _source_proxy_guard = install_move_queue_relay_worker(&worker_root, &marker, false);

    let replacement_directory = tempfile::tempdir().unwrap();
    let replacement_root = replacement_directory.path().join(MOVE_QUEUE_SESSION_ID);
    let replacement_marker = replacement_directory.path().join("replacement.marker");
    let _replacement_proxy_guard =
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
    session.target = Some(Box::new(target.clone()));
    session.native_session_id = None;
    session.state = SessionState::Running;
    let state = State {
        sessions: [(session.id.clone(), session)].into_iter().collect(),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    crate::database::save_materialized_session(&MaterializedSession::empty(MOVE_QUEUE_SESSION_ID))
        .unwrap();

    let mut controller = Controller { config, state };
    let selection = MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
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
        quota_stop_reason: Some("the weekly quota is exhausted".into()),
        quota_continuation: Some(if goal {
            mj_core::state::QuotaMoveContinuation::ResumeGoal
        } else {
            mj_core::state::QuotaMoveContinuation::Prompt
        }),
        prepared_destination: None,
        accepted_preparation: None,
        acknowledge_interruption: false,
        workspace_transfer: None,
        handoff: None,
        in_place: true,
        source_checkpoint_only: false,
        operation_id: "move-queue-replay".into(),
        selection,
        source_profile_id: "codex".into(),
        source_target_template_id: "local-bare".into(),
        source_target: None,
        source_native_session_id: None,
        inherited_resource_allocation: None,
        inherited_additional_mounts: Vec::new(),
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
    let retry_id = format!("quota-retry-{}", operation.operation_id);
    let accepted_ordinal = {
        let relay = DurableRelay::open(&worker_root, MOVE_QUEUE_SESSION_ID, "1.0.0").unwrap();
        let snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
            &fs::read(worker_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        )
        .unwrap();
        let accepted = snapshot.handled_commands[&retry_id].accepted_ordinal;
        drop(relay);
        accepted
    };
    // Resume from the durable intent, as a replacement daemon does.
    operation = crate::database::load_move_operation(MOVE_QUEUE_SESSION_ID)
        .unwrap()
        .unwrap();

    // Hard-won: 581ae9e: a missing legacy Move archive prevented ordinary attachment.
    let snapshot_path = worker_root.join(mj_core::relay::RELAY_STATE_FILE);
    let mut saved: mj_core::relay::RelaySnapshot =
        serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    saved.legacy_command_retention = true;
    saved.command_retention.clear();
    // Hard-won: f30ad64: cleaning a completed Move's pins cannot reconcile
    // the unrelated pending Move whose archive is unavailable.
    saved.command_retention.insert(
        "completed-move-before-crash".into(),
        [retry_id.clone()].into(),
    );
    fs::write(&snapshot_path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let archive_path = operation.restore_artifact().unwrap().archive_path.clone();
    let archive = fs::read(&archive_path).unwrap();
    let spec = controller.reconnect_command(MOVE_QUEUE_SESSION_ID).unwrap();
    for corrupt in [false, true] {
        if corrupt {
            fs::write(&archive_path, b"corrupt archive").unwrap();
        } else {
            fs::remove_file(&archive_path).unwrap();
        }
        runtime.block_on(async {
            let connection = crate::session_manager::StandaloneSession::connect_command(
                &spec,
                MOVE_QUEUE_SESSION_ID,
            )
            .await
            .unwrap();
            assert_eq!(
                connection.operational().store_id.as_deref(),
                Some(destination_store_id.as_str())
            );
        });
        let saved: mj_core::relay::RelaySnapshot =
            serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
        assert!(saved.legacy_command_retention);
        assert!(
            !saved
                .command_retention
                .contains_key("completed-move-before-crash")
        );
    }
    fs::write(&archive_path, archive).unwrap();

    runtime
        .block_on(controller.admit_move_queue(&mut operation, &RefusingExecutor("move preflight")))
        .unwrap();
    assert!(operation.queue_admission_finished);
    assert!(!move_owns_session(MOVE_QUEUE_SESSION_ID));
    let accepted_commands = |log: &str| {
        log.lines()
            .filter(|line| {
                line.starts_with(&format!("{retry_id} "))
                    || line.starts_with("queued-image ")
                    || line.starts_with("queued-config ")
            })
            .count()
    };
    let before_finished_retry = fs::read_to_string(marker.with_extension("observed")).unwrap();
    // A crash after the durable completion record may leave cleanup unfinished.
    // Such a retry must never resubmit commands after their pins were released.
    runtime
        .block_on(controller.admit_move_queue(&mut operation, &RefusingExecutor("move preflight")))
        .unwrap();
    assert_eq!(
        accepted_commands(&fs::read_to_string(marker.with_extension("observed")).unwrap()),
        accepted_commands(&before_finished_retry)
    );
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
    // Hard-won: a turn starting after preparation must still block source replacement.
    assert!(!operational.safe_to_move(
        HarnessKind::Codex,
        mj_core::activity::MovePurpose::QuotaRetry
    ));
    if std::env::var(MOVE_QUEUE_GOAL).as_deref() == Ok("active") {
        assert!(
            operational.goal.active(),
            "quota Prompt preserves the active goal"
        );
    }
    if goal {
        assert!(operational.goal.running());
        assert!(!operational.goal.resumable_after_quota());
    } else {
        assert_eq!(
            operational
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.command_id.as_str()),
            Some(retry_id.as_str())
        );
    }
    if !goal {
        assert_eq!(
            operational
                .queued_prompts
                .iter()
                .map(|queued| queued.command_id.as_str())
                .collect::<Vec<_>>(),
            ["queued-image", "queued-config"]
        );
    }
    assert_eq!(
        operation.quota_continuation,
        Some(if goal {
            mj_core::state::QuotaMoveContinuation::ResumeGoal
        } else {
            mj_core::state::QuotaMoveContinuation::Prompt
        })
    );

    let receipts: mj_core::relay::RelaySnapshot = serde_json::from_slice(
        &fs::read(worker_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
    )
    .unwrap();
    let accepted: Vec<_> = receipts
        .handled_commands
        .iter()
        .map(|(id, receipt)| (id.clone(), receipt.accepted_ordinal))
        .collect();
    let retried: Vec<_> = accepted.iter().filter(|(id, _)| id == &retry_id).collect();
    assert_eq!(
        retried.len(),
        1,
        "continuation must be admitted only once: {accepted:?}"
    );
    assert_eq!(retried[0].1, accepted_ordinal);
    let image_ordinal = accepted
        .iter()
        .find(|(id, _)| id == "queued-image")
        .unwrap()
        .1;
    let config_ordinal = accepted
        .iter()
        .find(|(id, _)| id == "queued-config")
        .unwrap()
        .1;
    assert!(accepted_ordinal < image_ordinal && image_ordinal < config_ordinal);
    // Acknowledged journal bodies may be gone; the daemon's durable projection
    // owns the conversation and the notices it acknowledged.
    let journal = serde_json::to_string(
        &crate::database::load_materialized_session(MOVE_QUEUE_SESSION_ID)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(journal.contains("because the weekly quota is exhausted"));
    assert!(journal.contains("Continuing the interrupted work"));
    assert!(!journal.contains("The interrupted prompt was not replayed"));

    let replacement_target = TargetLocator::LocalBare {
        worker_root: replacement_root,
    };
    controller
        .state
        .sessions
        .get_mut(MOVE_QUEUE_SESSION_ID)
        .expect("move queue session exists")
        .target = Some(Box::new(replacement_target.clone()));
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
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(session_id),
    }));
    let state = State {
        sessions: [(session_id.into(), session.clone())].into_iter().collect(),
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
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
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
    let first =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection.clone(),
                &ProcessExecutor,
            ))
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
    let second =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &ProcessExecutor,
            ))
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

// Hard-won: #1579: retrying a Move that was still restoring the session on its destination failed
// after a guessed 5 s wait for a relay actor that session legitimately does not have.
#[test]
fn move_preparation_reports_a_session_without_a_relay_actor_as_unavailable_at_once() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let manager = crate::session_manager::spawn_session_manager().unwrap();
        let started = tokio::time::Instant::now();
        let snapshot = super::refresh_move_source(&manager.control, "restoring-on-destination")
            .await
            .expect("an absent actor is an unavailable source, not an error");
        assert!(snapshot.is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the lookup must not wait for an actor that never appears"
        );
        manager.shutdown.shutdown().await.unwrap();
    });
}

#[cfg(unix)]
// Hard-won: 6b51b56d6: Move preparation waited for a background checkpoint's mutation permit.
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
        let checkpoint_owner = crate::controller::test_support::worker_test_permit(
            MOVE_QUEUE_SESSION_ID,
            "background recovery checkpoint",
        )
        .unwrap()
        .unwrap();
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            super::refresh_move_source(&manager.control, MOVE_QUEUE_SESSION_ID),
        )
        .await
        .expect("preparation must not wait for checkpoint ownership")
        .unwrap();
        assert!(snapshot.is_none());
        drop(checkpoint_owner);
        manager.shutdown.shutdown().await.unwrap();
    });
}

#[test]
fn in_place_eligibility_requires_same_target_mounts_and_allocation() {
    let mut source = raw_session_on("local-bare", "/home/dev/project");
    source.state = SessionState::Running;
    source.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: PathBuf::from("/tmp/worker"),
    }));
    source.additional_mounts = Vec::new();
    source.resource_allocation = None;
    let baseline = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
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
    let ssh = |host: &str| mj_core::config::TargetTemplate::SshBare {
        ssh: mj_core::config::SshConnection {
            host: host.into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        },
        permissions: mj_core::config::PermissionMode::Guardian,
        workspace_prefix: PathBuf::from("/srv/work"),
    };
    let targets: std::collections::BTreeMap<String, mj_core::config::TargetTemplate> = [
        ("local-bare", mj_core::config::TargetTemplate::LocalBare),
        ("local-bare-two", mj_core::config::TargetTemplate::LocalBare),
        ("ssh-bare", ssh("builder")),
        ("ssh-bare-alias", ssh("builder")),
        ("ssh-bare-other", ssh("other")),
    ]
    .into_iter()
    .map(|(id, template)| (id.to_string(), template))
    .collect();
    let cases: Vec<(&str, mj_core::state::MoveSelection, bool, bool)> = vec![
        ("profile only", baseline.clone(), false, true),
        (
            "another local bare target",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
                target_template_id: Some("local-bare-two".into()),
                ..baseline.clone()
            },
            false,
            true,
        ),
        (
            "different target template",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
                target_template_id: Some("ssh-bare".into()),
                ..baseline.clone()
            },
            false,
            false,
        ),
        (
            "different attached mounts",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
                additional_mounts: another_mount,
                ..baseline.clone()
            },
            false,
            false,
        ),
        (
            "different resource allocation",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
                resource_allocation: another_allocation,
                ..baseline.clone()
            },
            false,
            false,
        ),
        (
            "clearing absent resources is a no-op",
            mj_core::state::MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                workspace: Default::default(),
                clear_resource_allocation: true,
                ..baseline.clone()
            },
            false,
            true,
        ),
        ("sub-agent", baseline.clone(), true, false),
    ];
    for (label, selection, is_subagent, expected) in cases {
        assert_eq!(
            super::move_in_place_for_record(&source, &selection, &targets, is_subagent, None)
                .unwrap(),
            expected,
            "{label}"
        );
    }

    // A source with no provisioned target has no environment to keep, and a
    // stopped source has already lost it.
    let mut without_target = source.clone();
    without_target.target = None;
    assert!(
        !super::move_in_place_for_record(&without_target, &baseline, &targets, false, None)
            .unwrap()
    );
    let mut stopped = source.clone();
    stopped.state = SessionState::Stopped;
    assert!(!super::move_in_place_for_record(&stopped, &baseline, &targets, false, None).unwrap());
    let mut disconnected = source.clone();
    disconnected.state = SessionState::Disconnected;
    assert!(
        super::move_in_place_for_record(&disconnected, &baseline, &targets, false, None).unwrap()
    );

    // A cross-target Move that began admitting its queue already runs on the
    // destination, which matches the selection exactly. Its retry keeps the
    // decision it carried out instead of calling the destination in place.
    let mut admitting = source_recovery_operation(&source);
    admitting.source_target_template_id = "ssh-bare".into();
    admitting.selection = baseline.clone();
    admitting.phase = MovePhase::Failed;
    admitting.queue_admission_started = true;
    // Hard-won: 2333717a3: a cross-target Move that began admitting its queue could no longer be retried
    assert!(
        !super::move_in_place_for_record(&source, &baseline, &targets, false, Some(&admitting))
            .unwrap()
    );
}

#[test]
fn a_move_that_only_changes_delegation_still_restarts_the_session() {
    use mj_core::subagent::SubagentPolicy;

    let mut source = raw_session_on("local-bare", "/home/dev/project");
    source.state = SessionState::Running;
    source.subagents = Some(SubagentPolicy::Native);
    let same = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
        clear_resource_allocation: false,
        session_id: source.id.clone(),
        profile_id: Some(source.last_profile.clone()),
        target_template_id: Some(source.target_template_id.clone()),
        additional_mounts: Some(source.additional_mounts.clone()),
        resource_allocation: source.resource_allocation.clone(),
    };
    assert!(super::move_changes_nothing(&source, &same));

    // A viewer may send the current policy explicitly; that changes nothing.
    let explicit_same = mj_core::state::MoveSelection {
        subagents: Some(SubagentPolicy::Native),
        workspace: Default::default(),
        ..same.clone()
    };
    assert!(super::move_changes_nothing(&source, &explicit_same));

    let to_mjolnir = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: Some(SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        }),
        workspace: Default::default(),
        ..same.clone()
    };
    assert!(!super::move_changes_nothing(&source, &to_mjolnir));

    let mut stopped = source.clone();
    stopped.state = SessionState::Stopped;
    assert!(!super::move_changes_nothing(&stopped, &same));
}

#[test]
fn in_place_move_retains_running_and_parked_children_and_roleless_refusal_names_live_ones() {
    let mut parent = raw_session_on("local-bare", "/workspace");
    parent.state = SessionState::Running;
    parent.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    let mut running = parent.clone();
    running.id = "child-running".into();
    running.state = SessionState::Running;
    // The Move text names a child as the session list does, so a rename wins
    // over the task name the relation holds.
    running.session_title_override = Some("Renamed implementer".into());
    let mut parked = parent.clone();
    parked.id = "child-parked".into();
    parked.state = SessionState::Parked;
    parked.session_title_override = Some("review the tests".into());
    let relation = |child_session_id: &str, task_name: &str| mj_core::subagent::SubagentRecord {
        kind: mj_core::subagent::SubagentKind::Worker,
        reviewer_launch: None,
        child_session_id: child_session_id.into(),
        parent_session_id: parent.id.clone(),
        task_name: task_name.into(),
        profile_id: "codex".into(),
        model: None,
        effort: None,
        working_directory: PathBuf::new(),
        initial_prompt: task_name.into(),
        request_key: format!("request-{child_session_id}"),
        created_at: "2026-10-05T00:00:00Z".into(),
        noticed_turn: None,
        reported_finish: None,
        handback_tool: false,
    };
    let state = State {
        sessions: [parent.clone(), running.clone(), parked.clone()]
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect(),
        subagents: [
            relation(&running.id, "implement the change"),
            relation(&parked.id, "review the tests"),
        ]
        .into_iter()
        .map(|child| (child.child_session_id.clone(), child.into()))
        .collect(),
        ..Default::default()
    };

    let children = super::move_children(&state, &parent.id);
    assert_eq!(children.len(), 2);
    assert!(children.iter().any(|child| {
        child.child_session_id == running.id
            && child.state == mj_core::subagent::InPlaceSubagentState::Running
    }));
    assert!(children.iter().any(|child| {
        child.child_session_id == parked.id
            && child.state == mj_core::subagent::InPlaceSubagentState::Parked
    }));
    assert!(!super::should_stop_move_subagents(true, true));
    assert!(super::should_stop_move_subagents(false, true));
    assert!(super::should_stop_move_subagents(true, false));

    let refusal =
        super::roleless_move_children_error(&super::live_move_children(&state, &parent.id))
            .unwrap();
    assert!(refusal.contains("Renamed implementer"));
    assert!(refusal.contains("child-running"));
    assert!(!refusal.contains("implement the change"));
    assert!(!refusal.contains("review the tests"));

    // A live oracle alone never refuses a Move to a harness without parent
    // tools; the ordinary stop path stops it.
    let mut oracle_only = state.clone();
    oracle_only.subagents.get_mut(&running.id).unwrap().kind =
        mj_core::subagent::SubagentKind::Oracle;
    assert!(super::live_move_children(&oracle_only, &parent.id).is_empty());
    // Nor is it ever named to the primary after an in-place swap.
    assert_eq!(super::move_children(&oracle_only, &parent.id).len(), 1);

    let mut parked_only = state;
    parked_only.sessions.get_mut(&running.id).unwrap().state = SessionState::Parked;
    assert!(super::live_move_children(&parked_only, &parent.id).is_empty());
    assert!(super::roleless_move_children_error(&[]).is_none());
}

#[test]
fn in_place_drain_waits_for_mutations_and_durable_effects_but_not_wait_or_list() {
    use mj_core::subagent::{SubagentToolAction as Action, SubagentToolRequest};
    let request = |action| SubagentToolRequest {
        originating_command_id: None,
        request_id: "request".into(),
        created_at_ms: 1,
        action,
    };
    let mutating = [
        Action::Spawn {
            no_github_credentials: false,
            task_name: "task".into(),
            instructions: "work".into(),
            task_type: mj_core::subagent::SubagentTaskType::Straightforward,
            working_directory: PathBuf::new(),
            context: None,
            files: Vec::new(),
        },
        Action::SendInput {
            child_session_id: "child".into(),
            message: "continue".into(),
        },
        Action::SendMessage {
            child_session_id: "child".into(),
            message: "note".into(),
        },
        Action::CloseAgent {
            child_session_id: "child".into(),
        },
        Action::LegacyInterruptAgent {
            child_session_id: "child".into(),
        },
    ];
    for action in mutating {
        assert!(super::subagent_mutations_pending(&[request(action)], false));
    }
    let observations = [Action::ListAgents, Action::LegacyWaitAgents];
    assert!(
        observations
            .iter()
            .all(|action| !super::subagent_mutations_pending(&[request(action.clone())], false,))
    );
    assert!(super::subagent_mutations_pending(&[], true));
    assert!(!super::subagent_mutations_pending(&[], false));
}

#[test]
fn ssh_bare_targets_share_an_environment_only_on_the_same_connection() {
    let ssh = |host: &str| mj_core::config::TargetTemplate::SshBare {
        ssh: mj_core::config::SshConnection {
            host: host.into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        },
        permissions: mj_core::config::PermissionMode::Guardian,
        workspace_prefix: PathBuf::from("/srv/work"),
    };
    let targets: std::collections::BTreeMap<String, mj_core::config::TargetTemplate> = [
        ("a", ssh("builder")),
        ("b", ssh("builder")),
        ("c", ssh("other")),
        ("local", mj_core::config::TargetTemplate::LocalBare),
    ]
    .into_iter()
    .map(|(id, template)| (id.to_string(), template))
    .collect();
    assert!(super::bare_targets_share_environment(&targets, "a", "b"));
    assert!(!super::bare_targets_share_environment(&targets, "a", "c"));
    assert!(!super::bare_targets_share_environment(
        &targets, "a", "local"
    ));
    assert!(!super::bare_targets_share_environment(
        &targets, "a", "missing"
    ));
}

#[test]
fn move_raw_checkout_uses_the_same_accepted_bundle_transition_plan() {
    let (checkout, _remote_parent, _remote) = checkout_with_network_remote();
    let bundle = ProjectBundle {
        primary_repo: "bifrost-source".into(),
        repositories: vec![ProjectRepository {
            id: "bifrost-source".into(),
            github: None,
            local: Some(checkout.path().canonicalize().unwrap()),
            destination: PathBuf::from("accepted/layout"),
            git_ref: None,
        }],
    };
    let mut source = raw_session_on("local-bare", &checkout.path().to_string_lossy());
    source.bundle_id = "bifrost".into();
    source.project = Some(
        crate::project_catalog::snapshot(&bundle, &ProcessExecutor, false)
            .unwrap()
            .into(),
    );
    let controller = Controller {
        config: resume_compatibility_config(),
        state: State {
            sessions: [(source.id.clone(), source.clone())].into_iter().collect(),
            ..State::default()
        },
    };

    let conversion = controller
        .validate_move_destination_paths(&source, "podman", &ProcessExecutor)
        .unwrap()
        .unwrap();

    assert_eq!(conversion.project.bundle, bundle);
    assert_eq!(conversion.repository_id, "bifrost-source");
    assert_eq!(conversion.destination, PathBuf::from("accepted/layout"));
    assert_eq!(
        conversion.source.fetch_url,
        crate::controller::test_support::FIXTURE_FETCH_URL
    );
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.clone(),
    }));
    crate::database::save_session(&session).unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, directory.path());
    let mut controller = Controller {
        config,
        state: State {
            sessions: [(session.id.clone(), session.clone())]
                .into_iter()
                .collect(),
            ..State::default()
        },
    };
    struct StopFails {
        cancelled: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl CommandExecutor for StopFails {
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
                    &crate::controller::test_support::session_admission(&session.id),
                    &session.id,
                    &executor,
                    &manager.control,
                    &mut operation,
                    &mut MoveSourceRelay::default(),
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

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        assert!(command.purpose.contains("Move workspace"));
        let request = serde_json::from_reader(input)?;
        Ok(CommandOutput {
            status: 0,
            stdout: serde_json::to_vec(&mj_worker::move_workspace::execute(request)?)?,
            stderr: Vec::new(),
        })
    }
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        if command.program == "git" {
            return crate::controller::test_support::FixtureRemoteExecutor {
                remote: self.remote.clone(),
            }
            .execute(command);
        }
        if command.program != "podman" {
            return ProcessExecutor.execute(command);
        }
        let stdout: &[u8] = if command.args.iter().any(|argument| argument == "--version") {
            b"podman version 5.4.2\n"
        } else {
            br#"{"host":{"security":{"rootless":true},"serviceIsRemote":false,"idMappings":{"uidmap":[{"container_id":0,"host_id":1000,"size":1},{"container_id":1,"host_id":100000,"size":65536}]}}}"#
        };
        Ok(CommandOutput {
            status: 0,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(session_id),
    }));
    let state = State {
        sessions: [(session_id.into(), session)].into_iter().collect(),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    let mut controller = Controller { config, state };
    let selection = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
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

    let first =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection.clone(),
                &executor,
            ))
            .unwrap();

    let preview = first.conversion.as_deref().expect("a conversion preview");
    assert_eq!(
        preview.fetch_url,
        crate::controller::test_support::FIXTURE_FETCH_URL
    );
    assert_eq!(preview.branch.as_deref(), Some("master"));
    assert_eq!(preview.default_branch, "master");
    // The accepted bundle's destination determines where the checkout lands.
    assert_eq!(
        preview.destination,
        mj_core::targets::new_container_workspace(session_id)
            .unwrap()
            .join("project")
    );
    assert_eq!(preview.unpushed_commits, 0);
    assert_eq!(preview.untracked_files, 0);
    assert!(preview.host_checkout_retained);

    // A live agent keeps editing, so the dirty counts must not be able to
    // invalidate a confirmation the person is still reading.
    fs::write(repository.path().join("agent-edit.txt"), "written\n").unwrap();
    let second =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &executor,
            ))
            .unwrap();

    assert_eq!(
        second.conversion.as_deref().unwrap().untracked_files,
        1,
        "the preview reports the new file"
    );
    assert_eq!(first.fingerprint, second.fingerprint);

    // The source is bare, so it has no size to keep. The Move gives the
    // destination the same size `mj new` would: the host's remembered size,
    // else the baseline. Without it container admission refuses the Move and
    // an accepted one would run unbounded (#1585).
    let container_size = |cpus: u64, gib: u64| {
        Some(SessionResourceAllocation::Container {
            cpus,
            memory_bytes: gib << 30,
        })
    };
    assert_eq!(first.selection.resource_allocation, container_size(4, 8));
    assert!(!first.selection.clear_resource_allocation);

    // A retry carries the size it was sealed with, so the default is only
    // chosen for a selection that names none.
    let mut from_scratch = first.selection.clone();
    from_scratch.resource_allocation = None;
    controller.state.remember_container_size(
        "local",
        mj_core::state::HostContainerSize {
            cpus: 12,
            memory_bytes: 24 << 30,
        },
    );
    // Clearing inherited sizing on a container destination means the
    // destination default, and the sealed selection carries that size rather
    // than two instructions that contradict each other.
    let mut cleared = from_scratch.clone();
    cleared.clear_resource_allocation = true;
    let cleared =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                cleared,
                &executor,
            ))
            .unwrap();
    assert_eq!(
        cleared.selection.resource_allocation,
        container_size(12, 24)
    );
    assert!(
        !cleared.selection.clear_resource_allocation,
        "the sealed selection carries the destination size"
    );

    // An explicit size is the destination's size, and the default never
    // replaces it.
    let mut explicit = from_scratch;
    explicit.resource_allocation = container_size(6, 16);
    let explicit =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                explicit,
                &executor,
            ))
            .unwrap();
    assert_eq!(
        explicit.selection.resource_allocation,
        container_size(6, 16)
    );
}

/// Answers for a new Move destination container, recording the restore its
/// worker is asked to run instead of running it. Git runs for real, with the
/// fixture's network URL rewritten wherever it contacts a remote.
#[cfg(unix)]
struct RecordedMoveRestore {
    remote: PathBuf,
    restore: std::sync::Mutex<Option<Vec<mj_core::move_workspace::WorkspaceRepository>>>,
}

#[cfg(unix)]
impl CommandExecutor for RecordedMoveRestore {
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
        if command.program == "git" {
            return crate::controller::test_support::FixtureRemoteExecutor {
                remote: self.remote.clone(),
            }
            .execute(command);
        }
        // The image carries this build's worker, so nothing is uploaded.
        let (status, stdout) = match command.purpose.as_str() {
            "read the container image's worker build" => {
                (0, mj_core::worker_build::worker_build_id().as_bytes())
            }
            "detect target platform" => (1, &b""[..]),
            _ => (0, &b""[..]),
        };
        Ok(CommandOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }
    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        assert_eq!(command.purpose, "prepare or verify Move workspace");
        let mj_core::move_workspace::WorkspaceCommand::Restore { repositories, .. } =
            serde_json::from_reader(input)?
        else {
            panic!("the destination is only asked to restore");
        };
        *self.restore.lock().unwrap() = Some(repositories);
        Ok(CommandOutput {
            status: 0,
            stdout: b"null".to_vec(),
            stderr: Vec::new(),
        })
    }
}

// Hard-won: DSH campaign 2026-10-23 (C-1): a Move from a managed clone into a container cloned into the session-id folder
#[cfg(unix)]
#[test]
fn a_managed_clone_moved_into_a_container_is_restored_where_it_was_cloned() {
    use crate::controller::checkpoint::HOST_CHECKOUT_REPOSITORY;
    use crate::controller::test_support::{
        checkpoint_archive_input, managed_clone_session, write_checkpoint_archive_input,
    };
    use mj_checkpoint::archive::{
        GitCollectionSpec, GitHistoryMode, SystemGit, collect_git_metadata_snapshot,
    };
    let short = "a_managed_clone_moved_into_a_container_is_restored_where_it_was_cloned";
    if !isolated_test_child(&test_name(short), "MJ_MOVE_CLONE_DESTINATION_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (repository, _remote_parent, remote) = checkout_with_network_remote();
    let executor = RecordedMoveRestore {
        remote,
        restore: Default::default(),
    };
    let session_id = MOVE_QUEUE_SESSION_ID;
    // Accepted as `mj new --project-directory` accepts it, so the bundle
    // calls the repository after its remote (`repo`), not `project`.
    let (bundle_id, project) = crate::project_catalog::accept_directory(
        &mj_core::config::TargetTemplate::LocalBare,
        repository.path(),
        &executor,
    )
    .unwrap();
    let mut source = managed_clone_session(repository.path(), session_id);
    source.bundle_id = bundle_id;
    source.project = Some(project.into());
    source.state = SessionState::Closing;
    source.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(session_id),
    }));
    crate::database::save_session(&source).unwrap();
    let mut controller = Controller {
        config: resume_compatibility_config(),
        state: State {
            sessions: [(session_id.into(), source.clone())].into_iter().collect(),
            ..State::default()
        },
    };

    // The handoff records the source's layout, which names the clone's own
    // folder, the session id, as the repository's destination.
    let layout = controller
        .session_export_layout(session_id, &executor)
        .unwrap();
    let repositories = layout
        .repositories
        .iter()
        .map(|repository| {
            collect_git_metadata_snapshot(
                &SystemGit,
                &Path::new(&layout.workspace_root).join(&repository.relative_destination),
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.relative_destination.clone(),
                    history: GitHistoryMode::NoBundle,
                    origin_override: None,
                },
            )
            .unwrap()
        })
        .collect();
    let archives = tempfile::tempdir().unwrap();
    let mut operation = source_recovery_operation(&source);
    operation.selection.target_template_id = Some("podman".into());
    operation.phase = MovePhase::ResumingDestination;
    operation.handoff = Some(write_checkpoint_archive_input(
        archives.path(),
        session_id,
        &checkpoint_archive_input(session_id, 0, repositories, Vec::new()),
    ));
    operation.workspace_transfer = Some(
        controller
            .new_workspace_transfer(
                session_id,
                &operation.operation_id,
                Default::default(),
                &executor,
            )
            .unwrap(),
    );
    // The destination stage is named after what the capture holds.
    let captured = &operation
        .workspace_transfer
        .as_ref()
        .unwrap()
        .controller_stage;
    std::fs::create_dir_all(captured).unwrap();
    std::fs::write(captured.join("move-workspace.json"), b"{}").unwrap();
    crate::database::save_move_operation(&operation).unwrap();

    // Resume reshapes the record for the container before provisioning it.
    let conversion = crate::controller::worktree::plan_raw_to_workspace_for_session(
        &source,
        &controller.state.checkout(session_id).unwrap(),
        &controller.config,
        &executor,
    )
    .unwrap();
    let container_workspace = mj_core::targets::new_container_workspace(session_id).unwrap();
    let record = controller.state.sessions.get_mut(session_id).unwrap();
    record.target_template_id = "podman".into();
    record.target_runtime = Some((&controller.config.targets["podman"]).into());
    record.container_workspace = Some(container_workspace.clone());
    record.target = None;
    crate::controller::worktree::apply_raw_to_workspace(record, &conversion).unwrap();

    let resource =
        mj_core::targets::move_resource_name(session_id, &operation.operation_id).unwrap();
    let bundle = controller
        .move_destination_bundle(session_id)
        .unwrap()
        .expect("a Move into a container clones the session's bundle");
    let plan = crate::targets::provision_plan_named(
        &crate::controller::backend::backend_target(
            &controller.config.targets["podman"],
            None,
            Default::default(),
        )
        .unwrap(),
        session_id,
        &bundle,
        &[],
        Some(&container_workspace),
        &resource,
        None,
    )
    .unwrap();
    let cloned = plan
        .commands
        .iter()
        .filter(|command| command.purpose.starts_with("clone "))
        .map(|command| PathBuf::from(command.args.last().unwrap()))
        .collect::<Vec<_>>();

    let destination = controller.state.sessions.get_mut(session_id).unwrap();
    destination.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: resource,
        workspace_storage: Default::default(),
        borrowed_from: None,
    }));
    controller
        .restore_move_workspace(session_id, &executor)
        .unwrap();
    let restored = executor
        .restore
        .lock()
        .unwrap()
        .take()
        .expect("the destination restores the transferred workspace");

    assert_eq!(cloned, [container_workspace.join("repo")]);
    assert_eq!(
        restored,
        [mj_core::move_workspace::WorkspaceRepository {
            base_commit: None,
            // The transfer names the repository as the source captured it.
            id: HOST_CHECKOUT_REPOSITORY.into(),
            root: cloned[0].clone(),
        }]
    );
}

/// Runs a Move restore on this machine: Git with the fixture's network URL
/// rewritten wherever it contacts a remote, every other command for real, and
/// the Move worker in this process.
#[cfg(unix)]
struct LocalMoveRestore {
    remote: PathBuf,
}

#[cfg(unix)]
impl CommandExecutor for LocalMoveRestore {
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
        if command.program == "git" {
            return crate::controller::test_support::FixtureRemoteExecutor {
                remote: self.remote.clone(),
            }
            .execute(command);
        }
        ProcessExecutor.execute(command)
    }
    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        assert_eq!(command.purpose, "prepare or verify Move workspace");
        let request = serde_json::from_reader(input)?;
        Ok(CommandOutput {
            status: 0,
            stdout: serde_json::to_vec(&mj_worker::move_workspace::execute(request)?)?,
            stderr: Vec::new(),
        })
    }
}

// Hard-won: DSH campaign 2026-10-23 (C-1 reverse): a container session moved to a local checkout failed unless its repository was named project
#[cfg(unix)]
#[test]
fn a_container_session_moved_to_a_local_checkout_is_restored_into_it() {
    use crate::controller::test_support::test_git;
    use crate::controller::worktree::{
        PrimaryCheckoutRequirement, apply_workspace_to_raw, create_managed_worktree,
    };
    use mj_core::move_workspace::WorkspaceRepository;
    let short = "a_container_session_moved_to_a_local_checkout_is_restored_into_it";
    if !isolated_test_child(&test_name(short), "MJ_MOVE_CHECKOUT_DESTINATION_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let (repository, _remote_parent, remote) = checkout_with_network_remote();
    let executor = LocalMoveRestore {
        remote: remote.clone(),
    };
    let session_id = MOVE_QUEUE_SESSION_ID;
    // The bundle calls the repository after its remote (`repo`), not
    // `project`, and keeps the local repository a checkout can be made from.
    let (bundle_id, project) = crate::project_catalog::accept_directory(
        &mj_core::config::TargetTemplate::LocalBare,
        repository.path(),
        &executor,
    )
    .unwrap();
    let mut source = checkpoint_test_session(session_id);
    source.bundle_id = bundle_id;
    source.project = Some(project.into());
    source.state = SessionState::Closing;
    source.container_workspace =
        Some(mj_core::targets::new_container_workspace(session_id).unwrap());
    source.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: mj_core::targets::resource_name(session_id).unwrap(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    }));
    crate::database::save_session(&source).unwrap();
    let mut controller = Controller {
        config: resume_compatibility_config(),
        state: State {
            sessions: [(session_id.into(), source.clone())].into_iter().collect(),
            ..State::default()
        },
    };

    // The container's checkout and the work it carries, captured under the
    // ids the source's layout gives, as the source's worker captures it.
    let layout = controller
        .session_export_layout(session_id, &executor)
        .unwrap();
    assert_eq!(layout.primary_repository, "repo");
    let container = tempfile::tempdir().unwrap();
    test_git(
        container.path(),
        &["clone", &remote.to_string_lossy(), "repo"],
    );
    let container_checkout = container.path().join("repo");
    fs::write(
        container_checkout.join("nested/file.txt"),
        "edited in the container\n",
    )
    .unwrap();
    fs::write(container_checkout.join("container-note.txt"), "untracked\n").unwrap();
    let mut operation = source_recovery_operation(&source);
    operation.selection.target_template_id = Some("local-bare".into());
    operation.phase = MovePhase::ResumingDestination;
    let transfer = controller
        .new_workspace_transfer(
            session_id,
            &operation.operation_id,
            Default::default(),
            &executor,
        )
        .unwrap();
    mj_worker::move_workspace::capture(
        &layout
            .repositories
            .iter()
            .map(|repository| WorkspaceRepository {
                base_commit: None,
                id: repository.id.clone(),
                root: container_checkout.clone(),
            })
            .collect::<Vec<_>>(),
        &Default::default(),
        &transfer.controller_stage,
    )
    .unwrap();
    operation.workspace_transfer = Some(transfer);
    operation.checkpoint = Some(CheckpointMetadata {
        archive_path: "unused.hel.zip".into(),
        sha256: "0".repeat(64),
        created_at: "2026-08-14T12:00:00Z".into(),
        event_frontier: 0,
    });
    crate::database::save_move_operation(&operation).unwrap();

    // Resume gives the session its checkout before the transfer fills it.
    let conversion = controller
        .plan_workspace_to_raw(&source, "local-bare", &executor)
        .unwrap();
    create_managed_worktree(
        &executor,
        &conversion.worktree,
        None,
        PrimaryCheckoutRequirement::Any,
    )
    .unwrap();
    let record = controller.state.sessions.get_mut(session_id).unwrap();
    record.target_template_id = "local-bare".into();
    record.target_runtime = Some((&controller.config.targets["local-bare"]).into());
    record.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(session_id),
    }));
    apply_workspace_to_raw(record, &conversion);

    controller
        .restore_move_workspace(session_id, &executor)
        .unwrap();

    let checkout = &conversion.worktree.worktree_root;
    assert_eq!(
        fs::read_to_string(checkout.join("nested/file.txt")).unwrap(),
        "edited in the container\n"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("container-note.txt")).unwrap(),
        "untracked\n"
    );
}

/// A transport can answer `--version` while rejecting a required option.
struct RsyncWithoutPartialDir(GitWithPodmanPreflightExecutor);

impl CommandExecutor for RsyncWithoutPartialDir {
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

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        self.0.execute_with_stdin(command, input)
    }
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let runs_rsync = command.program == "rsync" || command.args.iter().any(|a| a == "rsync");
        if !runs_rsync {
            return self.0.execute(command);
        }
        let (status, stdout, stderr): (i32, &[u8], &[u8]) = if command
            .args
            .iter()
            .any(|a| a == "--partial-dir=.move-partial")
        {
            (1, b"", b"rsync: unrecognized option `--partial-dir'\n")
        } else {
            (0, b"rsync fixture\n", b"")
        };
        Ok(CommandOutput {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        })
    }
}

// Hard-won: 6fc2ed80: unsupported rsync transfer flags were discovered only after copying began
#[test]
#[cfg_attr(
    windows,
    ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
)]
fn a_move_is_blocked_before_copying_when_an_rsync_rejects_partial_dir() {
    let short = "a_move_is_blocked_before_copying_when_an_rsync_rejects_partial_dir";
    if !isolated_test_child(&test_name(short), "MJ_MOVE_RSYNC_PROBE_CHILD") {
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
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(session_id),
    }));
    let state = State {
        sessions: [(session_id.into(), session)].into_iter().collect(),
        ..State::default()
    };
    crate::database::save_state(&state).unwrap();
    let controller = Controller { config, state };
    let selection = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
        clear_resource_allocation: false,
        session_id: session_id.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some("podman".into()),
        additional_mounts: None,
        resource_allocation: None,
    };
    let executor = RsyncWithoutPartialDir(GitWithPodmanPreflightExecutor { remote });
    let preparation =
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection.clone(),
                &executor,
            ))
            .unwrap();

    let assessment = preparation.workspace.expect("a workspace transfer");
    for side in ["controller", "source", "Destination"] {
        assert!(
            assessment
                .blockers
                .iter()
                .any(|blocker| blocker.contains(side)
                    && blocker.contains("--partial-dir")
                    // The destination's rsync runs on its host, never in the image.
                    && !blocker.contains("container image")
                    && blocker.contains("destination host") == (side == "Destination")),
            "{side} rsync is not reported: {:?}",
            assessment.blockers
        );
    }
    assert!(
        selection
            .workspace
            .validate(&assessment)
            .unwrap_err()
            .to_string()
            .contains("--partial-dir"),
        "the transfer must refuse to start"
    );
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
    LATCH_CHECKPOINT_ONLY, LATCH_RELAY_ANSWER_PROMPTS, LATCH_RELAY_SESSION,
};

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
            .execute_move(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                operation,
                None,
                executor,
                &channels.control,
                MoveSourceRelay::default(),
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        result
    })
}

/// A profile-only move on an unchanged target replaces the harness inside the
/// environment it found: nothing is destroyed or created, the worker root and
/// the workspace survive, and only the previous profile home is removed.
#[cfg(unix)]
// Hard-won: 6b51b56d6: unchanged native Moves ran provisioning checks and downloads.
#[test]
fn bare_move_preparation_preserves_an_explicit_resource_reset() {
    let name = test_name("bare_move_preparation_preserves_an_explicit_resource_reset");
    if !isolated_test_child(&name, "MJ_EMPTY_RESOURCE_RESET_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let mut selection =
        in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE).selection;
    selection.clear_resource_allocation = true;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &RefusingExecutor("profile-only Move needs no provisioning commands"),
            ))
            .unwrap();
    assert!(prepared.in_place);
    assert!(prepared.selection.clear_resource_allocation);
    let mut source = fixture.controller.state.sessions[LATCH_RELAY_SESSION].clone();
    source.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 4,
        memory_bytes: 8 << 30,
    });
    let mut selection = prepared.selection;
    selection.clear_resource_allocation = true;
    assert!(
        !super::move_in_place_for_record(
            &source,
            &selection,
            &fixture.controller.config.targets,
            false,
            None
        )
        .unwrap(),
        "clearing a legacy CPU allocation changes the environment"
    );
}

#[cfg(unix)]
// Hard-won: #1479: an explicitly resumed Move forced later profile changes into a blocked transfer.
#[test]
fn profile_move_after_explicit_resume_starts_fresh_without_transferring_files() {
    let name =
        test_name("profile_move_after_explicit_resume_starts_fresh_without_transferring_files");
    if !isolated_test_child(&name, "MJ_MOVE_AFTER_RESUME_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::Cancelled;
    operation.handoff = Some(fixture.checkpoint.clone());
    operation.checkpoint = Some(fixture.checkpoint.clone());
    // These are the durable fields written after an explicit Resume succeeds.
    operation.queue_admission_finished = true;
    operation.error = Some("Recovered through an explicit Resume operation".into());
    crate::database::save_move_operation(&operation).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Both retrying the old destination and choosing a different profile must
    // start from the live worker, never from the historical handoff archive.
    for profile in [IN_PLACE_DESTINATION_PROFILE, IN_PLACE_SOURCE_PROFILE] {
        let mut selection = operation.selection.clone();
        selection.profile_id = Some(profile.into());
        let prepared = runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &RefusingExecutor("a profile-only Move must not scan or provision storage"),
            ))
            .unwrap();
        assert!(prepared.in_place);
        assert!(prepared.workspace.is_none());
        assert!(!prepared.stops_subagents);
        assert_ne!(prepared.operation_id, operation.operation_id);
        assert!(
            super::move_in_place_for_record(
                &fixture.controller.state.sessions[LATCH_RELAY_SESSION],
                &prepared.selection,
                &fixture.controller.config.targets,
                false,
                Some(&operation),
            )
            .unwrap()
        );
    }
    let stored = crate::database::load_move_operation(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert!(!stored.offers_retry());
    assert!(!stored.checkpoint_retained());
    assert!(!stored.holds_source_environment());
    assert!(super::ViewerMoveRecovery::from_operation(&stored).is_none());
    assert!(!stored.retains_checkpoint());
    assert_eq!(stored.retained_archives().count(), 0);
    assert!(fixture.checkpoint.archive_path.exists());

    // A cancellation before Resume, or after destination queue admission,
    // still owns recovery. Loading it must preserve the retry and its archive.
    for (started, finished) in [(false, false), (true, false), (true, true)] {
        operation.queue_admission_started = started;
        operation.queue_admission_finished = finished;
        crate::database::save_move_operation(&operation).unwrap();
        let stored = crate::database::load_move_operation(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert!(stored.offers_retry());
        let api = super::ViewerMoveRecovery::from_operation(&stored).unwrap();
        assert!(api.checkpoint_retained);
        assert!(stored.retains_checkpoint());
    }
}

#[cfg(unix)]
// Hard-won: move-8c670999: a failed in-place Move made every later same-target Move a workspace transfer
#[test]
fn in_place_move_that_failed_at_the_seal_leaves_the_next_same_target_move_in_place() {
    let name = test_name(
        "in_place_move_that_failed_at_the_seal_leaves_the_next_same_target_move_in_place",
    );
    if !isolated_test_child(&name, "MJ_MOVE_SEAL_FAILURE_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    // The Move's own handoff is a separate file; the session keeps its checkpoint.
    let mut handoff = fixture.checkpoint.clone();
    handoff.archive_path = fixture
        .checkpoint
        .archive_path
        .with_file_name("move-handoff.hel.zip");
    fs::copy(&fixture.checkpoint.archive_path, &handoff.archive_path).unwrap();
    // What the Move stores before it asks the worker to seal, and what is still
    // stored when the worker refuses: the source never stopped and has moved on.
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::ClosingSource;
    operation.handoff = Some(handoff.clone());
    operation.checkpoint = Some(fixture.checkpoint.clone());
    crate::database::save_move_operation(&operation).unwrap();
    let outcome = fixture
        .controller
        .finish_move_result(
            &mut operation,
            Err(anyhow::anyhow!(
                "seal verified checkpoint for close: close does not match the current checkpoint cut"
            )),
            &RefusingExecutor("recording a failed Move runs no commands"),
        )
        .unwrap();
    assert_eq!(outcome.outcome, "failed");
    let stored = crate::database::load_move_operation(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert_eq!(stored.phase, MovePhase::Failed);
    assert!(!stored.checkpoint_retained());
    assert!(!stored.offers_retry());
    assert!(
        !handoff.archive_path.exists(),
        "the stale handoff is released"
    );
    assert!(
        fixture.checkpoint.archive_path.exists(),
        "the session's own checkpoint is not the Move's to delete"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // A later Move that differs from the failed one in a defaulted field is
    // still a profile-only Move: it never inspects or transfers the workspace.
    let mut selection = operation.selection.clone();
    selection.clear_resource_allocation = true;
    for _ in 0..2 {
        let prepared = runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection.clone(),
                &RefusingExecutor("a same-environment Move must not inspect the workspace"),
            ))
            .unwrap();
        assert!(prepared.in_place);
        assert!(prepared.workspace.is_none());
        assert_ne!(prepared.operation_id, operation.operation_id);
        // The second pass starts from the row a daemon that shipped earlier
        // left behind, which startup releases the same way.
        crate::database::save_move_operation(&MoveOperation {
            handoff: Some(handoff.clone()),
            checkpoint: Some(fixture.checkpoint.clone()),
            ..stored.clone()
        })
        .unwrap();
        crate::controller::reconcile_managed_checkpoint_archives().unwrap();
        let released = crate::database::load_move_operation(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert!(!released.checkpoint_retained());
        fixture.controller.state = crate::database::load_state().unwrap();
    }
}

#[cfg(unix)]
// Hard-won: #1470: native delegation without managed children blocked automatic quota Moves.
#[test]
fn roleless_in_place_move_refuses_live_children_but_allows_parked_children() {
    let name = test_name("roleless_in_place_move_refuses_live_children_but_allows_parked_children");
    if !isolated_test_child(&name, "MJ_MOVE_ROLELESS_CHILDREN_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let parent_id = LATCH_RELAY_SESSION;
    let parent = fixture
        .controller
        .state
        .sessions
        .get_mut(parent_id)
        .unwrap();
    parent.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    crate::database::save_session(parent).unwrap();
    let mut empty_selection =
        in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE).selection;
    empty_selection.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let empty =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                empty_selection.clone(),
                &ProcessExecutor,
            ))
            .unwrap();
    assert!(empty.in_place);
    assert!(!empty.stops_subagents);

    let mut child = fixture.controller.state.sessions[parent_id].clone();
    child.id = "live-child".into();
    child.state = SessionState::Running;
    child.session_title_override = Some("finish the migration".into());
    child.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: fixture._directory.path().join("live-child"),
    }));
    let relation = mj_core::subagent::SubagentRecord {
        kind: mj_core::subagent::SubagentKind::Worker,
        reviewer_launch: None,
        child_session_id: child.id.clone(),
        parent_session_id: parent_id.into(),
        task_name: "spawned as an audit".into(),
        profile_id: "codex".into(),
        model: None,
        effort: None,
        working_directory: PathBuf::new(),
        initial_prompt: "finish the migration".into(),
        request_key: "spawn-live-child".into(),
        created_at: "2026-10-05T00:00:00Z".into(),
        noticed_turn: None,
        reported_finish: None,
        handback_tool: false,
    };
    crate::database::save_subagent_session(&child, &relation).unwrap();
    fixture
        .controller
        .state
        .sessions
        .insert(child.id.clone(), child.clone());
    fixture
        .controller
        .state
        .subagents
        .insert(child.id.clone(), relation.clone().into());
    let mut selection =
        in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE).selection;
    selection.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    let error =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection.clone(),
                &ProcessExecutor,
            ))
            .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("finish the migration"), "{message}");
    assert!(message.contains("live-child"), "{message}");
    assert!(message.contains("mj-agents parent tools"), "{message}");

    child.state = SessionState::Parked;
    crate::database::save_session(&child).unwrap();
    fixture
        .controller
        .state
        .sessions
        .insert(child.id.clone(), child);
    let prepared =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &ProcessExecutor,
            ))
            .unwrap();
    assert!(prepared.in_place);
    assert!(
        prepared.stops_subagents,
        "a destination without parent tools stops the parked children"
    );

    fixture
        .controller
        .state
        .subagents
        .get_mut("live-child")
        .unwrap()
        .kind = mj_core::subagent::SubagentKind::Oracle;
    let oracle =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                empty_selection.clone(),
                &ProcessExecutor,
            ))
            .unwrap();
    assert!(oracle.stops_subagents, "parked oracles are stopped too");
    fixture
        .controller
        .state
        .sessions
        .get_mut("live-child")
        .unwrap()
        .state = SessionState::Stopped;
    let stopped =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                empty_selection,
                &ProcessExecutor,
            ))
            .unwrap();
    assert!(!stopped.stops_subagents);
    let late_child = fixture
        .controller
        .state
        .sessions
        .get_mut("live-child")
        .unwrap();
    late_child.state = SessionState::Parked;
    crate::database::save_session(late_child).unwrap();
    // A child admitted after the empty preview is checked under the drained
    // worker queue before an automatic Move can run the child-stop hook.
    let mut automatic = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    automatic.selection = empty.selection.clone();
    automatic.configuration_fingerprint = fixture
        .controller
        .move_configuration_fingerprint(&automatic.selection)
        .unwrap();
    automatic.quota_stop_reason = Some("weekly quota exhausted".into());
    let executor = recording_executor();
    let error = run_in_place_move(
        &mut fixture.controller,
        &mut automatic,
        &fixture.worker_root,
        &executor,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("automatic quota Move would stop existing sub-agents"));
    assert_eq!(executor.move_stop_calls(), 0);
    assert_eq!(
        fixture.controller.state.sessions[parent_id].state,
        SessionState::Running
    );
    let mut manual = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    manual.selection = empty.selection.clone();
    manual.configuration_fingerprint = fixture
        .controller
        .move_configuration_fingerprint(&manual.selection)
        .unwrap();
    manual.accepted_preparation = Some(Box::new(empty));
    let error = run_in_place_move(
        &mut fixture.controller,
        &mut manual,
        &fixture.worker_root,
        &executor,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("sub-agents appeared after preparation"));
    assert_eq!(executor.move_stop_calls(), 0);
}

#[cfg(unix)]
#[test]
fn recovery_from_in_place_closing_source_keeps_children_and_skips_stop_hook() {
    let name =
        test_name("recovery_from_in_place_closing_source_keeps_children_and_skips_stop_hook");
    if std::env::var_os("MJ_MOVE_IN_PLACE_DRAIN_RECOVERY_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_IN_PLACE_DRAIN_RECOVERY_CHILD", "1")
            // The recovery path nests the checkpoint and relay futures deeply.
            // Their debug-build frames exceed libtest's default 2 MiB stack.
            .env("RUST_MIN_STACK", "8388608")
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
    let parent = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    parent.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    crate::database::save_session(parent).unwrap();
    let parent = controller.state.sessions[LATCH_RELAY_SESSION].clone();
    for (child_id, task_name, child_state) in [
        (
            "running-child",
            "implement the change",
            SessionState::Running,
        ),
        ("parked-child", "review the tests", SessionState::Parked),
    ] {
        let mut child = parent.clone();
        child.id = child_id.into();
        child.state = child_state;
        child.subagents = None;
        child.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: _directory.path().join(child_id),
        }));
        let relation = mj_core::subagent::SubagentRecord {
            kind: mj_core::subagent::SubagentKind::Worker,
            reviewer_launch: None,
            child_session_id: child_id.into(),
            parent_session_id: parent.id.clone(),
            task_name: task_name.into(),
            profile_id: IN_PLACE_SOURCE_PROFILE.into(),
            model: None,
            effort: None,
            working_directory: PathBuf::new(),
            initial_prompt: task_name.into(),
            request_key: format!("spawn-{child_id}"),
            created_at: "2026-10-05T00:00:00Z".into(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: false,
        };
        crate::database::save_subagent_session(&child, &relation).unwrap();
        controller.state.sessions.insert(child.id.clone(), child);
        controller
            .state
            .subagents
            .insert(relation.child_session_id.clone(), relation.into());
    }
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::ClosingSource;
    crate::database::save_move_operation(&operation).unwrap();
    let source_relay = seed_source_relay(&worker_root);
    let executor = recording_executor();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let outcome = controller
            .recover_move_managed_controlled(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                operation,
                &executor,
                &channels.control,
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        outcome.unwrap()
    });

    assert_eq!(outcome.outcome, "completed", "{outcome:?}");
    assert_eq!(executor.move_stop_calls(), 0);
    let persisted = crate::database::load_state().unwrap();
    assert_eq!(
        persisted.sessions["running-child"].state,
        SessionState::Running
    );
    assert_eq!(
        persisted.sessions["parked-child"].state,
        SessionState::Parked
    );
    assert!(persisted.subagents.contains_key("running-child"));
    assert!(persisted.subagents.contains_key("parked-child"));
    let relay_snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
        &fs::read(worker_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
    )
    .unwrap();
    let context = relay_snapshot.pending_prompt_context.unwrap().text;
    assert!(context.contains("running-child; running"), "{context}");
    assert!(context.contains("parked-child; parked"), "{context}");
    assert!(context.contains("wait that was in progress"), "{context}");
}

/// The oracle choice a Move was sealed with is what the destination session
/// records, in memory and in the store, and a sealed retry must send it again.
#[cfg(unix)]
#[test]
fn in_place_move_records_its_oracle_selection_on_the_session() {
    let name = test_name("in_place_move_records_its_oracle_selection_on_the_session");
    if std::env::var_os("MJ_MOVE_ORACLE_SELECTION_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_ORACLE_SELECTION_CHILD", "1")
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
    let choice = mj_core::config::SubagentChoice {
        model: "advisor".into(),
        effort: Some("high".into()),
    };
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    // Turning Oracle off while naming a model keeps the model for later.
    operation.selection.oracle = Some(false);
    operation.selection.oracle_choice = Some(choice.clone());
    operation.configuration_fingerprint = controller
        .move_configuration_fingerprint(&operation.selection)
        .unwrap();
    run_in_place_move(
        &mut controller,
        &mut operation,
        &worker_root,
        &recording_executor(),
    )
    .unwrap();

    let moved = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert!(!moved.oracle);
    assert_eq!(moved.oracle_choice, Some(Box::new(choice.clone())));
    let stored = crate::database::load_session_record(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert!(!stored.oracle);
    assert_eq!(stored.oracle_choice, Some(Box::new(choice)));
}

/// A Move that stopped the oracle leaves the same entry a suspend does, and
/// the primary's first prompt after the swap says so without naming the oracle.
#[cfg(unix)]
#[test]
fn in_place_move_tells_the_primary_its_oracle_was_stopped() {
    let name = test_name("in_place_move_tells_the_primary_its_oracle_was_stopped");
    if std::env::var_os("MJ_MOVE_ORACLE_STOPPED_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_ORACLE_STOPPED_CHILD", "1")
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
    crate::database::record_stopped_subagents(
        LATCH_RELAY_SESSION,
        &[mj_core::subagent::StoppedSubagent::oracle()],
    )
    .unwrap();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    run_in_place_move(
        &mut controller,
        &mut operation,
        &worker_root,
        &recording_executor(),
    )
    .unwrap();

    let relay_snapshot: mj_core::relay::RelaySnapshot = serde_json::from_slice(
        &fs::read(worker_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
    )
    .unwrap();
    let context = relay_snapshot.pending_prompt_context.unwrap().text;
    assert!(
        context.contains(mj_core::subagent::STOPPED_ORACLE_LINE),
        "{context}"
    );
    assert!(!context.contains("child_session_id"), "{context}");
    assert!(
        crate::database::load_stopped_subagents(LATCH_RELAY_SESSION)
            .unwrap()
            .is_empty(),
        "the delivered entry is forgotten"
    );
}

#[cfg(unix)]
#[test]
fn in_place_move_reopens_message_only_queue_after_parent_policy_becomes_native() {
    let name =
        test_name("in_place_move_reopens_message_only_queue_after_parent_policy_becomes_native");
    if std::env::var_os("MJ_MOVE_MESSAGE_ONLY_REOPEN_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_MESSAGE_ONLY_REOPEN_CHILD", "1")
            .env(
                crate::controller::checkpoint::tests::LATCH_RECORD_SUBAGENT_ADMISSION,
                "1",
            )
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
    let source = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    source.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    crate::database::save_session(source).unwrap();

    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.selection.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    operation.configuration_fingerprint = controller
        .move_configuration_fingerprint(&operation.selection)
        .unwrap();
    run_in_place_move(
        &mut controller,
        &mut operation,
        &worker_root,
        &recording_executor(),
    )
    .unwrap();

    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].subagents,
        Some(mj_core::subagent::SubagentPolicy::Native)
    );
    let queue: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_root.join("subagents.json")).unwrap()).unwrap();
    assert_eq!(queue["mutating_admission_open"], true, "{queue}");
}

#[cfg(unix)]
#[test]
fn in_place_move_reopens_gate_after_lost_admission_close_reply() {
    let name = test_name("in_place_move_reopens_gate_after_lost_admission_close_reply");
    if std::env::var_os("MJ_MOVE_ADMISSION_CLOSE_LOST_REPLY_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_ADMISSION_CLOSE_LOST_REPLY_CHILD", "1")
            .env(
                crate::controller::checkpoint::tests::LATCH_RECORD_SUBAGENT_ADMISSION,
                "1",
            )
            .env(
                crate::controller::checkpoint::tests::LATCH_DROP_ADMISSION_CLOSE_REPLY,
                "1",
            )
            .env(
                crate::controller::checkpoint::tests::LATCH_CHECKPOINT_ONLY,
                "1",
            )
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
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    crate::database::save_session(&controller.state.sessions[LATCH_RELAY_SESSION]).unwrap();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    let executor = recording_executor();
    let source_relay = seed_source_relay(&worker_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let result = controller
            .execute_move(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                &mut operation,
                None,
                &executor,
                &channels.control,
                MoveSourceRelay::default(),
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        result
    });
    let error = result.unwrap_err();
    assert!(!format!("{error:#}").is_empty());
    assert!(
        source_relay
            .join("drop-admission-close-reply-once")
            .exists(),
        "the fake source worker persisted the close before dropping its reply; Move error: {error:#}"
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert_eq!(executor.move_stop_calls(), 0);
    let queue: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_root.join("subagents.json")).unwrap()).unwrap();
    assert_eq!(queue["mutating_admission_open"], true, "{queue}");
}

/// The same source, failing after the close, gets its queue back.
#[cfg(unix)]
#[test]
fn in_place_move_reopens_the_queue_of_a_native_source_after_a_failure() {
    let name = test_name("in_place_move_reopens_the_queue_of_a_native_source_after_a_failure");
    if std::env::var_os("MJ_MOVE_NATIVE_REOPEN_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_NATIVE_REOPEN_CHILD", "1")
            .env(
                crate::controller::checkpoint::tests::LATCH_RECORD_SUBAGENT_ADMISSION,
                "1",
            )
            .env(
                crate::controller::checkpoint::tests::LATCH_DROP_ADMISSION_CLOSE_REPLY,
                "1",
            )
            .env(
                crate::controller::checkpoint::tests::LATCH_CHECKPOINT_ONLY,
                "1",
            )
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
    let source = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    source.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    crate::database::save_session(source).unwrap();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    let executor = recording_executor();
    let source_relay = seed_source_relay(&worker_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let result = controller
            .execute_move(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                &mut operation,
                None,
                &executor,
                &channels.control,
                MoveSourceRelay::default(),
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        result
    });
    let error = result.unwrap_err();
    assert!(
        source_relay
            .join("drop-admission-close-reply-once")
            .exists(),
        "the Native source's queue was closed before the failure; Move error: {error:#}"
    );
    let queue: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_root.join("subagents.json")).unwrap()).unwrap();
    assert_eq!(queue["mutating_admission_open"], true, "{queue}");
}

/// A daemon that stopped while an in-place Move was stopping its source,
/// before the seal, leaves the source `Closing` with an open relay. Recovery
/// returns it to `Running`; an uncancelled Move then resumes and finishes,
/// and a cancelled one reopens the sub-agent gate it closed.
#[cfg(unix)]
// Hard-won: 555274842: recovery left an in-place Move whose source stop was interrupted for a manual retry
#[test]
fn in_place_closing_source_recovery_reopens_gate_after_restoring_running_session() {
    let name =
        test_name("in_place_closing_source_recovery_reopens_gate_after_restoring_running_session");
    if std::env::var_os("MJ_MOVE_CLOSING_SOURCE_REOPEN_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_CLOSING_SOURCE_REOPEN_CHILD", "1")
            .env(
                crate::controller::checkpoint::tests::LATCH_RECORD_SUBAGENT_ADMISSION,
                "1",
            )
            .env(
                crate::controller::checkpoint::tests::LATCH_CHECKPOINT_ONLY,
                "1",
            )
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    // The store keeps a recorded cancellation for the same operation, so the
    // uncancelled case runs first.
    for cancelled in [false, true] {
        let InPlaceFixture {
            _directory,
            worker_root,
            mut controller,
            ..
        } = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
        let parent = controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap();
        parent.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        });
        parent.state = SessionState::Closing;
        crate::database::save_session(parent).unwrap();
        fs::write(
            worker_root.join("subagents.json"),
            br#"{"requests":{},"results":{},"mutating_admission_open":false}"#,
        )
        .unwrap();
        let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
        operation.phase = MovePhase::ClosingSource;
        operation.cancellation_requested = cancelled;
        crate::database::save_move_operation(&operation).unwrap();
        let source_relay = seed_source_relay(&worker_root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(async {
            let channels = start_source_relay_manager(&source_relay);
            let outcome = controller
                .recover_move_managed_controlled(
                    &crate::controller::test_support::session_admission(
                        &operation.selection.session_id,
                    ),
                    operation,
                    &recording_executor(),
                    &channels.control,
                )
                .await;
            channels.shutdown.shutdown().await.unwrap();
            outcome.unwrap()
        });
        let recovered = &controller.state.sessions[LATCH_RELAY_SESSION];
        assert_eq!(recovered.state, SessionState::Running);
        if !cancelled {
            // The source is running again, so recovery resumes the Move as it
            // does when startup first finds the source running.
            assert_eq!(outcome.outcome, "completed", "{outcome:?}");
            assert_eq!(recovered.last_profile, IN_PLACE_DESTINATION_PROFILE);
            continue;
        }
        assert_eq!(outcome.outcome, "cancelled", "{outcome:?}");
        let queue: serde_json::Value =
            serde_json::from_slice(&fs::read(worker_root.join("subagents.json")).unwrap()).unwrap();
        assert_eq!(queue["mutating_admission_open"], true, "{queue}");
    }
}

#[cfg(unix)]
#[test]
fn in_place_move_with_pre_v32_worker_stops_children_and_reports_fallback() {
    let name = test_name("in_place_move_with_pre_v32_worker_stops_children_and_reports_fallback");
    if std::env::var_os("MJ_MOVE_OLD_RELAY_FALLBACK_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_OLD_RELAY_FALLBACK_CHILD", "1")
            .env(
                crate::controller::checkpoint::tests::LATCH_RELAY_MAX_PROTOCOL,
                "31",
            )
            .env(
                crate::controller::checkpoint::tests::LATCH_CHECKPOINT_ONLY,
                "1",
            )
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    fixture.controller.state.sessions[LATCH_RELAY_SESSION].subagents =
        Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
        });
    crate::database::save_session(&fixture.controller.state.sessions[LATCH_RELAY_SESSION]).unwrap();
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    let executor = recording_executor();
    run_in_place_move(
        &mut fixture.controller,
        &mut operation,
        &fixture.worker_root,
        &executor,
    )
    .unwrap();
    assert_eq!(executor.move_stop_calls(), 1);
    assert!(executor.notices().iter().any(|notice| {
        notice.contains("relay protocol 31")
            && notice.contains("protocol 32 is required")
            && notice.contains("stopping sub-agents")
    }));
}

#[cfg(unix)]
// Hard-won: ea0608d4d: cutoff verification rejected superseded session commits.
#[test]
fn profile_move_preserves_a_workspace_without_later_history() {
    let name = test_name("profile_move_preserves_a_workspace_without_later_history");
    if std::env::var_os("MJ_CUTOFF_MOVE_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_CUTOFF_MOVE_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let git = crate::controller::test_support::test_git;
    let base = git(&fixture.checkout, &["rev-parse", "HEAD"]);
    git(&fixture.checkout, &["remote", "remove", "origin"]);
    fs::write(fixture.checkout.join("session.txt"), "session work").unwrap();
    git(&fixture.checkout, &["add", "session.txt"]);
    git(
        &fixture.checkout,
        &["commit", "--allow-empty", "-m", "session work"],
    );
    let superseded = git(&fixture.checkout, &["rev-parse", "HEAD"]);
    git(
        &fixture.checkout,
        &["commit", "--amend", "-m", "amended session work"],
    );
    fs::write(fixture.checkout.join("session.txt"), "discarded work").unwrap();
    git(
        &fixture.checkout,
        &["stash", "push", "-m", "discarded session stash"],
    );
    let dropped_stash = git(&fixture.checkout, &["rev-parse", "refs/stash"]);
    git(&fixture.checkout, &["stash", "drop"]);
    let head = git(&fixture.checkout, &["rev-parse", "HEAD"]);
    let bundle = crate::controller::test_support::local_bundle(&fixture.checkout);
    fixture
        .controller
        .config
        .bundles
        .insert("project".into(), bundle);
    let record = fixture
        .controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    record.bundle_id = "project".into();
    record.checkout = Some(Box::new(mj_core::remote_git::ExactCheckout {
        repository_id: "project".into(),
        commit: base.clone(),
        branch: Some("main".into()),
        no_later_history: true,
    }));
    record.launch_base = Some(base);
    record.state = SessionState::Closing;
    crate::database::save_session(record).unwrap();
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::ResumingDestination;
    operation.checkpoint = Some(fixture.checkpoint.clone());
    operation.recovery_session =
        Some(fixture.controller.state.sessions[LATCH_RELAY_SESSION].clone());
    crate::database::save_move_operation(&operation).unwrap();
    let executor = recording_executor();
    run_in_place_move(
        &mut fixture.controller,
        &mut operation,
        &fixture.worker_root,
        &executor,
    )
    .unwrap();
    for commit in [&superseded, &dropped_stash] {
        git(&fixture.checkout, &["cat-file", "-e", commit]);
    }
    assert_eq!(git(&fixture.checkout, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&fixture.checkout, &["remote"]), "");
    assert_eq!(
        git(
            &fixture.checkout,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/remotes",
                "refs/tags"
            ]
        ),
        ""
    );
    assert!(
        executor
            .purposes()
            .iter()
            .any(|purpose| purpose == "verify workspace history cutoff")
    );
    assert_eq!(
        std::fs::read(fixture.checkout.join("untracked.txt")).unwrap(),
        b"agent work"
    );
}

#[cfg(unix)]
// Hard-won: 6b51b56d6: profile-only Move deleted native state and provisioned the same worker again.
#[test]
fn in_place_profile_move_retains_native_state_and_installed_worker() {
    let name = test_name("in_place_profile_move_retains_native_state_and_installed_worker");
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
    use std::os::unix::fs::MetadataExt;
    let installed_worker_inode = fs::symlink_metadata(worker_root.join("hel")).unwrap().ino();
    let profile = worker_root.join("profile");
    fs::write(profile.join(".credentials.json"), b"source credentials").unwrap();
    fs::create_dir_all(profile.join("skills/synced/account")).unwrap();
    fs::write(
        profile.join("skills/synced/account/SKILL.md"),
        b"harness owned",
    )
    .unwrap();
    fs::create_dir_all(profile.join("skills/old-profile")).unwrap();
    fs::write(profile.join("skills/old-profile/SKILL.md"), b"source skill").unwrap();
    fs::create_dir_all(profile.join("projects/retained")).unwrap();
    let native_history = profile.join("projects/retained/native.jsonl");
    fs::write(&native_history, b"native history stays on its target\n").unwrap();
    let native_history_inode = fs::metadata(&native_history).unwrap().ino();
    let mut owned = managed_raw_session(mj_core::state::ManagedWorktreeTarget::Local)
        .managed_worktree
        .unwrap();
    owned.kind = mj_core::state::ManagedCheckoutKind::Clone;
    // An omitted Move override keeps a legacy policy even when the destination
    // profile defaults to Native.
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
        straightforward: None,
        subtle: None,
    });
    // The original seed repository is gone, but the session's independent
    // clone still exists under its recorded owner path.
    let project = checkout.with_file_name("retained-project");
    let retained_checkout = project.join(".mj/clones").join(LATCH_RELAY_SESSION);
    fs::create_dir_all(retained_checkout.parent().unwrap()).unwrap();
    fs::rename(&checkout, &retained_checkout).unwrap();
    let checkout = retained_checkout;
    let git = crate::controller::test_support::test_git;
    git(&checkout, &["init", "--initial-branch=master"]);
    git(&checkout, &["config", "user.name", "Move Test"]);
    git(&checkout, &["config", "user.email", "move@example.invalid"]);
    fs::write(checkout.join("tracked.txt"), b"committed source\n").unwrap();
    git(&checkout, &["add", "tracked.txt"]);
    git(&checkout, &["commit", "-m", "source checkout"]);
    owned.base_commit = Some(git(&checkout, &["rev-parse", "HEAD"]));
    owned.branch = "master".into();
    owned.worktree_root = checkout.clone();
    owned.source_repository = project.clone();
    owned.source_project_directory = project;
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .project_directory = Some(checkout.clone());
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .managed_worktree = Some(owned);
    crate::database::save_session(&controller.state.sessions[LATCH_RELAY_SESSION]).unwrap();
    let source_target = controller.state.sessions[LATCH_RELAY_SESSION]
        .target
        .clone();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.quota_stop_reason = Some("the weekly quota is exhausted".into());
    operation.quota_continuation = Some(mj_core::state::QuotaMoveContinuation::Prompt);
    operation.queue = ResumeQueueDisposition::Start;
    // This fixture's fake worker restores native state but does not export Git.
    // Model the verified, sealed source boundary before replacing the harness.
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .state = SessionState::Closing;
    operation.phase = MovePhase::ResumingDestination;
    operation.checkpoint = Some(checkpoint.clone());
    operation.recovery_session = Some(controller.state.sessions[LATCH_RELAY_SESSION].clone());
    crate::database::save_session(&controller.state.sessions[LATCH_RELAY_SESSION]).unwrap();
    crate::database::save_move_operation(&operation).unwrap();

    let executor = recording_executor();
    run_in_place_move(&mut controller, &mut operation, &worker_root, &executor).unwrap();

    let purposes = executor.purposes();
    for purpose in &purposes {
        let lowered = purpose.to_lowercase();
        assert!(
            !lowered.contains("remove exact")
                && !lowered.contains("podman")
                && !lowered.contains("docker")
                && !lowered.contains("create the session worker root")
                && !lowered.contains("clean")
                && !lowered.contains("restore target checkpoint")
                && !lowered.contains("install shared")
                && !lowered.contains("prepare exact managed harness")
                && !lowered.contains("inherit git setting"),
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
    assert_eq!(
        fs::read(profile.join("source-only.txt")).unwrap(),
        b"old harness state"
    );
    assert_eq!(
        fs::metadata(&native_history).unwrap().ino(),
        native_history_inode
    );
    assert_eq!(
        fs::read(&native_history).unwrap(),
        b"native history stays on its target\n"
    );
    assert_eq!(
        fs::symlink_metadata(worker_root.join("hel")).unwrap().ino(),
        installed_worker_inode
    );
    assert!(
        !profile.join(".credentials.json").exists(),
        "the destination has no credential file; do not keep the source's"
    );
    assert!(!profile.join("skills/old-profile").exists());
    assert_eq!(
        fs::read(profile.join("skills/synced/account/SKILL.md")).unwrap(),
        b"harness owned"
    );
    let staged_settings: serde_json::Value = serde_json::from_slice(
        &fs::read(worker_root.join("profile").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        staged_settings["profile"], "destination",
        "the destination profile must be staged in its place"
    );
    // The worker writes the `mj-agents` rules when it starts, so the staged
    // profile may have no allow list yet.
    assert!(
        !staged_settings["permissions"]["allow"]
            .as_array()
            .is_some_and(
                |allow| allow.contains(&serde_json::json!("mcp__mj-agents__list_profiles"))
            ),
        "Mjolnir delegation exposes no profile discovery tool"
    );
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
    assert_eq!(
        persisted.destination_target,
        source_target.as_deref().cloned()
    );
    assert_eq!(persisted.checkpoint.as_ref(), Some(&checkpoint));
    let moved = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert!(
        moved.publication.is_none(),
        "retaining the checkout must not assess remote publication"
    );
    assert_eq!(moved.state, SessionState::Running);
    assert_eq!(moved.last_profile, IN_PLACE_DESTINATION_PROFILE);
    assert_eq!(moved.target, source_target);
    assert_eq!(
        moved.subagents,
        Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None
        })
    );
    assert_eq!(
        crate::database::load_session_record(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap()
            .subagents,
        moved.subagents
    );
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
        journal.contains("in place because the weekly quota is exhausted"),
        "the move notice is missing from the destination journal: {journal}"
    );
}

#[cfg(unix)]
#[test]
fn restart_reuses_an_error_target_and_preserves_its_queued_command() {
    const CHILD: &str = "MJ_RESTART_IN_PLACE_CHILD";
    let name = test_name("restart_reuses_an_error_target_and_preserves_its_queued_command");
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env(CHILD, "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env(LATCH_RELAY_ANSWER_PROMPTS, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let queued_id = "restart-queued-prompt";
    let mut archive = crate::controller::test_support::checkpoint_archive_input(
        LATCH_RELAY_SESSION,
        fixture.checkpoint.event_frontier,
        Vec::new(),
        Vec::new(),
    );
    archive.session.harness_kind = HarnessKind::Claude;
    archive.session.profile_id = IN_PLACE_SOURCE_PROFILE.into();
    archive
        .canonical_session
        .queued_prompts
        .push(CanonicalQueuedPrompt {
            command_id: queued_id.into(),
            kind: CanonicalQueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type":"text","text":"queued before restart"})],
            queued_at_ms: 42,
        });
    fixture.checkpoint = crate::controller::test_support::write_checkpoint_archive_input(
        fixture.checkpoint.archive_path.parent().unwrap(),
        LATCH_RELAY_SESSION,
        &archive,
    );

    let temp_marker = fixture.worker_root.join("tmp").join("restart-marker");
    fs::create_dir_all(temp_marker.parent().unwrap()).unwrap();
    fs::write(&temp_marker, b"temporary target data").unwrap();
    let record = fixture
        .controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    record.state = SessionState::Error;
    record.last_error = Some("suspend lost its worker".into());
    record.last_checkpoint_error = Some("worker process is gone".into());
    record.checkpoint = Some(Box::new(fixture.checkpoint.clone()));
    crate::database::save_session(record).unwrap();

    let mut controller = fixture.controller;
    let executor = recording_executor();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let restored = runtime
        .block_on(crate::controller::test_support::worker_test_scope_async(
            LATCH_RELAY_SESSION,
            controller.restore_session_in_place_for_restart(
                LATCH_RELAY_SESSION,
                IN_PLACE_SOURCE_PROFILE,
                "local-bare",
                &executor,
            ),
        ))
        .unwrap()
        .unwrap();

    assert_eq!(restored.session_id, LATCH_RELAY_SESSION);
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].target,
        Some(Box::new(TargetLocator::LocalBare {
            worker_root: fixture.worker_root.clone()
        }))
    );
    assert_eq!(fs::read(&temp_marker).unwrap(), b"temporary target data");
    assert_eq!(
        fs::read(fixture.checkout.join("untracked.txt")).unwrap(),
        b"agent work"
    );

    let purposes = executor.purposes();
    assert_eq!(
        purposes
            .iter()
            .filter(|purpose| {
                purpose.as_str() == "reset the worker root for an in-place harness replacement"
            })
            .count(),
        1,
        "Restart should reset the worker inside the retained target: {purposes:?}"
    );
    assert!(
        purposes.iter().all(|purpose| {
            let purpose = purpose.to_lowercase();
            !purpose.contains("remove exact")
                && !purpose.contains("remove container")
                && !purpose.contains("container storage")
                && !purpose.contains("temporary volume")
                && !purpose.contains("create the session worker root")
        }),
        "Restart must not clean up or recreate its target: {purposes:?}"
    );

    let relay_state: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.worker_root.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
    )
    .unwrap();
    let queued_count = relay_state["queued_prompts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|queued| queued["command_id"] == queued_id)
        .count();
    let active_count = usize::from(relay_state["active_prompt"]["command_id"] == queued_id);
    let handled_count = usize::from(relay_state["handled_commands"].get(queued_id).is_some());
    assert!(
        queued_count + active_count + handled_count > 0
            && relay_state.to_string().contains("queued before restart"),
        "the queued command is restored into the retained relay with its identity and content: {relay_state:#}"
    );
}

#[cfg(unix)]
#[test]
fn restart_classifies_a_missing_target_as_a_preflight_fallback() {
    const CHILD: &str = "MJ_RESTART_MISSING_TARGET_CHILD";
    let name = test_name("restart_classifies_a_missing_target_as_a_preflight_fallback");
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let record = fixture
        .controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    record.state = SessionState::Closing;
    record.checkpoint = Some(Box::new(fixture.checkpoint.clone()));
    crate::database::save_session(record).unwrap();
    fs::remove_dir_all(&fixture.worker_root).unwrap();

    let mut controller = fixture.controller;
    let executor = recording_executor();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime
        .block_on(crate::controller::test_support::worker_test_scope_async(
            LATCH_RELAY_SESSION,
            controller.restore_session_in_place_for_restart(
                LATCH_RELAY_SESSION,
                IN_PLACE_SOURCE_PROFILE,
                "local-bare",
                &executor,
            ),
        ))
        .unwrap()
        .unwrap_err();

    assert!(
        matches!(error, crate::controller::InPlaceRestartError::Preflight(_)),
        "a missing target must be classified before restore begins: {error:?}"
    );
    assert!(
        executor.purposes().iter().all(|purpose| {
            !purpose.contains("reset the worker root")
                && !purpose.contains("remove exact")
                && !purpose.contains("remove container")
        }),
        "the failed preflight must leave cleanup to Restart's fallback"
    );
}

/// An in-place swap removes the session's staged home and never the profile
/// home it was staged from, which other sessions and the user share.
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
    // A plain Codex profile, which ran straight out of the user's own profile
    // home on a LocalBare target before every local session was staged.
    let InPlaceFixture {
        _directory,
        worker_root,
        source_home,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Codex, HarnessKind::Codex);
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    crate::database::save_move_operation(&operation).unwrap();

    let executor = recording_executor();
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

/// Two local bare targets are the same machine and the same worker storage, so
/// naming the other one is the in-place profile switch: the environment stays,
/// nothing is transferred, and the session's target template follows the choice.
#[cfg(unix)]
fn add_second_local_bare_target() {
    Config::update(|config| {
        config.targets.insert(
            "local-bare-two".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        Ok(())
    })
    .unwrap();
}

#[cfg(unix)]
#[test]
fn preparing_a_move_to_another_local_bare_target_keeps_the_environment() {
    let name = test_name("preparing_a_move_to_another_local_bare_target_keeps_the_environment");
    if !isolated_test_child(&name, "MJ_MOVE_LOCAL_BARE_PREPARE_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    add_second_local_bare_target();
    fixture.controller.config = Config::load().unwrap();
    let mut selection =
        in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE).selection;
    selection.target_template_id = Some("local-bare-two".into());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let prepared =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &ProcessExecutor,
            ))
            .unwrap();
    assert!(
        prepared.in_place,
        "a local bare to local bare Move is in place"
    );
    assert!(
        prepared.workspace.is_none(),
        "an in-place Move has no workspace to transfer or block"
    );
}

#[cfg(unix)]
#[test]
fn in_place_move_to_another_local_bare_target_records_the_new_template() {
    let name = test_name("in_place_move_to_another_local_bare_target_records_the_new_template");
    if std::env::var_os("MJ_MOVE_LOCAL_BARE_SWITCH_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_LOCAL_BARE_SWITCH_CHILD", "1")
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
    } = in_place_fixture(HarnessKind::Codex, HarnessKind::Codex);
    // A worker retained across a daemon upgrade needs a newer executable,
    // while its native session and environment can still stay in place.
    use std::os::unix::fs::MetadataExt;
    let previous_worker_inode = fs::symlink_metadata(worker_root.join("hel")).unwrap().ino();
    fs::write(
        worker_root.join("worker-build"),
        format!("0.1.0+{}\n", "0".repeat(40)),
    )
    .unwrap();
    add_second_local_bare_target();
    controller.config = Config::load().unwrap();
    let source_target = controller.state.sessions[LATCH_RELAY_SESSION]
        .target
        .clone();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.selection.target_template_id = Some("local-bare-two".into());
    operation.configuration_fingerprint = controller
        .move_configuration_fingerprint(&operation.selection)
        .unwrap();
    crate::database::save_move_operation(&operation).unwrap();

    let executor = recording_executor();
    run_in_place_move(&mut controller, &mut operation, &worker_root, &executor).unwrap();

    let moved = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(moved.state, SessionState::Running);
    assert_eq!(moved.last_profile, IN_PLACE_DESTINATION_PROFILE);
    assert_eq!(moved.target_template_id, "local-bare-two");
    assert_eq!(moved.target, source_target, "the environment is kept");
    assert_ne!(
        fs::symlink_metadata(worker_root.join("hel")).unwrap().ino(),
        previous_worker_inode
    );
    assert_eq!(
        fs::read(worker_root.join("profile/source-only.txt")).unwrap(),
        b"old harness state"
    );
    assert_eq!(
        fs::read(worker_root.join("build-cache.txt")).unwrap(),
        b"warm cache"
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

    let executor = recording_executor();
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
/// and the recovery guidance names the Resume command that restores it rather
/// than offering a retried Move that would rebuild a fresh destination.
#[cfg(unix)]
#[test]
fn in_place_move_failure_retains_environment_for_retry() {
    let name = test_name("in_place_move_failure_retains_environment_for_retry");
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
        ..recording_executor()
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
    assert!(outcome.recovery.as_deref().unwrap().contains("Retry Move"));

    let rolled_back = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(rolled_back.state, SessionState::Error);
    assert!(rolled_back.target.is_some());
    assert_eq!(rolled_back.checkpoint.as_deref(), Some(&checkpoint));
    // The record the swap replaced is back, harness and profile included.
    assert_eq!(rolled_back.last_profile, IN_PLACE_SOURCE_PROFILE);
    assert_eq!(rolled_back.harness_kind, HarnessKind::Claude);
    // Only the worker was stopped; the checkout and build cache survive.
    assert!(
        worker_root.exists(),
        "the rollback retains the environment: {}",
        worker_root.display()
    );
    let persisted = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(persisted.state, SessionState::Error);
    assert!(persisted.checkpoint.is_some());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let preparation = controller
            .prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                operation.selection.clone(),
                &ProcessExecutor,
            )
            .await
            .unwrap();
        assert!(
            preparation.in_place,
            "retry must retain its durable environment decision"
        );
        controller
            .execute_move(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                &mut operation,
                Some(&preparation),
                &recording_executor(),
                &channels.control,
                MoveSourceRelay::default(),
            )
            .await
            .unwrap();
        channels.shutdown.shutdown().await.unwrap();
    });
    assert_eq!(
        controller.state.sessions[LATCH_RELAY_SESSION].state,
        SessionState::Running
    );
    assert_eq!(
        fs::read(worker_root.join("build-cache.txt")).unwrap(),
        b"warm cache"
    );
}

/// A daemon restart before the swap started finishes the interrupted close of
/// a cancelled Move and retains the environment the move promised to keep.
/// An uncancelled Move goes on to restore; see
/// `in_place_move_killed_mid_flight_is_retried_from_its_handoff_after_restart`.
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
    let executor = recording_executor();
    // One daemon lifetime: seal the source and keep its target, exactly as the
    // first half of an in-place move does.
    runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
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
            .await;
        channels.shutdown.shutdown().await.unwrap();
        assert!(!deferred.unwrap());
    });
    let sealed = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(sealed.state, SessionState::Closing);
    assert!(sealed.target.is_some());

    operation.cancellation_requested = true;
    // The next daemon lifetime recovers the interrupted move.
    let outcome = runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let outcome = controller
            .recover_move_managed_controlled(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                operation,
                &executor,
                &channels.control,
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        outcome.unwrap()
    });

    let recovered = &controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(outcome.outcome, "cancelled");
    assert_eq!(
        outcome.error.as_deref(),
        Some("Move source sealed; environment retained for explicit retry")
    );
    assert_eq!(recovered.state, SessionState::Error);
    assert!(recovered.target.is_some());
    assert!(recovered.checkpoint.is_some());
    assert!(
        worker_root.exists(),
        "the recovered close retains its environment: {}",
        worker_root.display()
    );
}

/// Where the daemon died in W-2: right after the source was sealed
/// (`suspending`), after the Move recorded the destination phase but before
/// the swap changed the record (still `suspending`), while the destination
/// harness was being installed (`starting`), or while an earlier recovery was
/// setting up its retry (`error`, with the destination phase recorded).
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum DaemonKill {
    Suspending,
    SealedBeforeSwap,
    Starting,
    RetryRetained,
}

#[cfg(unix)]
const DAEMON_KILLS: [DaemonKill; 4] = [
    DaemonKill::Suspending,
    DaemonKill::SealedBeforeSwap,
    DaemonKill::Starting,
    DaemonKill::RetryRetained,
];

/// One daemon lifetime that seals an in-place Move's source and then dies at
/// `kill`, leaving exactly the durable state the killed daemon left. The
/// source has no earlier full checkpoint, like the campaign's session, so the
/// Move's handoff is the only archive it can restore from.
#[cfg(unix)]
fn in_place_move_killed_at(
    kill: DaemonKill,
    fixture: &mut InPlaceFixture,
    source_relay: &Path,
) -> MoveOperation {
    let controller = &mut fixture.controller;
    controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .checkpoint = None;
    crate::database::save_session(&controller.state.sessions[LATCH_RELAY_SESSION]).unwrap();
    let mut operation = in_place_operation(controller, IN_PLACE_DESTINATION_PROFILE);
    operation.phase = MovePhase::ClosingSource;
    // The campaign's Move was confirmed with `--allow-large-transfer`, which
    // an in-place Move never uses.
    operation.selection.workspace.acknowledge_large_transfer = true;
    crate::database::save_move_operation(&operation).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = start_source_relay_manager(source_relay);
        let deferred = controller
            .suspend_session_for_move(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                &recording_executor(),
                &channels.control,
                &mut operation,
                None,
                crate::controller::lifecycle::SourceTargetDisposition::RetainForInPlaceSwap,
                MoveSourceRelay::default(),
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        assert!(!deferred.unwrap());
    });
    let handoff = operation
        .handoff
        .clone()
        .expect("the seal records a handoff");
    assert!(handoff.archive_path.exists());
    assert!(
        handoff
            .archive_path
            .starts_with(mj_core::config::sessions_dir())
    );
    if let DaemonKill::SealedBeforeSwap | DaemonKill::Starting | DaemonKill::RetryRetained = kill {
        // What `execute_move` persisted before the swap: the destination
        // phase and the sealed source identity.
        operation.phase = MovePhase::ResumingDestination;
        operation.recovery_session = Some(controller.state.sessions[LATCH_RELAY_SESSION].clone());
        crate::database::save_move_operation(&operation).unwrap();
    }
    if let DaemonKill::Starting = kill {
        // What `restore_session_in_place` persisted before the kill: the
        // record's swap to the destination profile.
        let record = controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap();
        record.state = SessionState::Provisioning;
        record.last_profile = IN_PLACE_DESTINATION_PROFILE.into();
        crate::database::save_session(record).unwrap();
    }
    if let DaemonKill::RetryRetained = kill {
        // What the automatic retry persisted before the kill: the source
        // identity in `Error`, with its environment retained for the restore.
        let record = controller
            .state
            .sessions
            .get_mut(LATCH_RELAY_SESSION)
            .unwrap();
        record.state = SessionState::Error;
        crate::database::save_session(record).unwrap();
    }
    operation
}

/// The next daemon's startup for a killed Move: the archive sweep it runs
/// before anything else, then recovery of the Move from the durable record.
///
/// The new daemon's session manager has no actor for the sealed source, as in
/// the campaign, where its actor had already gone ("session manager stopped").
/// The seal is a durable fact of the Move, so recovery must not need the
/// sealed relay to answer again.
#[cfg(unix)]
fn restart_daemon_and_recover_move(controller: &mut Controller) -> mj_core::state::MoveOutcome {
    crate::controller::reconcile_managed_checkpoint_archives().unwrap();
    let operation = crate::database::load_move_operation(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let outcome = controller
            .recover_move_managed_controlled(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                operation,
                &recording_executor(),
                &channels.control,
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        outcome.unwrap()
    })
}

/// W-2: a daemon killed in the middle of an in-place Move leaves a Move the
/// next daemon finishes. Its startup sweep keeps the handoff the Move
/// restores from, and recovery restores from it without anyone asking. When
/// the person cancelled the Move, recovery stops instead: the published
/// guidance and the API agree that the checkpoint is retained, and the retry
/// the guidance names succeeds and clears the failure.
// Hard-won: 42de65fd: daemon death mid-swap left the in-place move without recoverable handoff state
// Hard-won: 555274842: recovery left an in-place Move interrupted after its seal in Error for a manual retry
#[test]
#[cfg(unix)]
fn in_place_move_killed_mid_flight_is_retried_from_its_handoff_after_restart() {
    let name =
        test_name("in_place_move_killed_mid_flight_is_retried_from_its_handoff_after_restart");
    if std::env::var_os("MJ_MOVE_KILLED_RETRY_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_KILLED_RETRY_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    for (kill, cancelled) in DAEMON_KILLS
        .into_iter()
        .flat_map(|kill| [(kill, false), (kill, true)])
    {
        let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
        let source_relay = seed_source_relay(&fixture.worker_root);
        let mut killed = in_place_move_killed_at(kill, &mut fixture, &source_relay);
        let handoff = killed.handoff.clone().unwrap();
        if cancelled {
            killed.cancellation_requested = true;
            crate::database::save_move_operation(&killed).unwrap();
        }

        let outcome = restart_daemon_and_recover_move(&mut fixture.controller);
        if !cancelled {
            assert_eq!(
                outcome.outcome, "completed",
                "{kill:?}: {:?}",
                outcome.error
            );
            let finished =
                crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
            assert_eq!(finished.state, SessionState::Running, "{kill:?}");
            assert_eq!(finished.last_profile, IN_PLACE_DESTINATION_PROFILE);
            assert_eq!(finished.last_error, None, "{kill:?}");
            assert_eq!(
                fs::read(fixture.worker_root.join("build-cache.txt")).unwrap(),
                b"warm cache"
            );
            continue;
        }
        assert_eq!(outcome.outcome, "cancelled", "{kill:?}");
        assert!(
            handoff.archive_path.exists(),
            "{kill:?}: the startup sweep deleted the Move's handoff {}",
            handoff.archive_path.display()
        );
        let recovered =
            crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
        assert_eq!(recovered.state, SessionState::Error, "{kill:?}");
        assert!(recovered.target.is_some(), "{kill:?}");
        assert_eq!(recovered.last_profile, IN_PLACE_SOURCE_PROFILE, "{kill:?}");
        let published = recovered.last_error.clone().unwrap();
        assert!(
            published.contains("Retry Move on the same target"),
            "{kill:?}: {published}"
        );
        let operation = crate::database::load_move_operation(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        let api = super::ViewerMoveRecovery::from_operation(&operation)
            .unwrap()
            .with_public_failure(Some(&published));
        assert_eq!(
            api.failure_notice.as_deref(),
            Some(published.as_str()),
            "the stored public recovery instructions must reach the conversation"
        );
        assert!(
            api.checkpoint_retained,
            "{kill:?}: the guidance promises a retry the API denies"
        );
        assert!(api.environment_retained, "{kill:?}");

        // The retry the guidance names, as the web sends it: without the
        // large-transfer acknowledgement, which an in-place Move does not use.
        let mut operation = operation;
        // A retry clears the cancellation, as the accepted retry request does;
        // a save alone keeps a recorded cancellation.
        crate::database::clear_move_cancellation_for_retry(LATCH_RELAY_SESSION).unwrap();
        operation.cancellation_requested = false;
        operation.error = None;
        operation.selection.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
        // What the dashboard seals: no attached directories, and an explicitly
        // cleared resource allocation.
        operation.selection.additional_mounts = Some(Vec::new());
        operation.selection.clear_resource_allocation = true;
        crate::database::save_move_operation(&operation).unwrap();
        let mut requested = operation.selection.clone();
        // Hard-won: move-f309ba7ac67ad183c55de12d66239891: the CLI could not retry a Move the dashboard sealed, because it cannot send attached directories or a cleared allocation.
        requested.additional_mounts = None;
        requested.clear_resource_allocation = false;
        // Hard-won: 5390a50d: omitted retry fields used source settings and stranded a sealed Move.
        requested.profile_id = None;
        requested.target_template_id = None;
        requested.subagents = None;
        requested.workspace.acknowledge_large_transfer = false;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let channels = crate::session_manager::spawn_session_manager().unwrap();
            let preparation = fixture
                .controller
                .prepare_move_session_controlled(
                    &crate::profile_catalog::ProfileCatalog::new(
                        tokio_util::sync::CancellationToken::new(),
                    ),
                    requested,
                    &ProcessExecutor,
                )
                .await
                .unwrap();
            assert_eq!(preparation.selection, operation.selection, "{kill:?}");
            assert!(preparation.in_place, "{kill:?}");
            let result = fixture
                .controller
                .execute_move(
                    &crate::controller::test_support::session_admission(
                        &operation.selection.session_id,
                    ),
                    &mut operation,
                    Some(&preparation),
                    &recording_executor(),
                    &channels.control,
                    MoveSourceRelay::default(),
                )
                .await;
            channels.shutdown.shutdown().await.unwrap();
            result
        });
        let outcome = fixture
            .controller
            .finish_move_result(&mut operation, result, &recording_executor())
            .unwrap();
        assert_eq!(
            outcome.outcome, "completed",
            "{kill:?}: {:?}",
            outcome.error
        );
        let finished = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
        assert_eq!(finished.state, SessionState::Running, "{kill:?}");
        assert_eq!(finished.last_profile, IN_PLACE_DESTINATION_PROFILE);
        assert_eq!(finished.last_error, None, "{kill:?}");
        assert_eq!(
            fs::read(fixture.worker_root.join("build-cache.txt")).unwrap(),
            b"warm cache"
        );
    }
}

/// W-2: when the archive a sealed Move restores from is gone, the Move record
/// says so, and every reader follows it. The published guidance and the API
/// stop promising a retry, the retry is refused with the reason before it
/// touches the session, and the session lands in `Error` where Destroy works,
/// never in `Closing` (suspending) with nothing to finish it.
// Hard-won: 42de65fd: lost in-place handoff was reported as success instead of destroyable failure
#[test]
#[cfg(unix)]
fn in_place_move_whose_handoff_is_lost_fails_truthfully_into_a_destroyable_state() {
    let name =
        test_name("in_place_move_whose_handoff_is_lost_fails_truthfully_into_a_destroyable_state");
    if std::env::var_os("MJ_MOVE_LOST_HANDOFF_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_LOST_HANDOFF_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    for kill in DAEMON_KILLS {
        let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
        let source_relay = seed_source_relay(&fixture.worker_root);
        let killed = in_place_move_killed_at(kill, &mut fixture, &source_relay);
        fs::remove_file(&killed.handoff.as_ref().unwrap().archive_path).unwrap();

        let outcome = restart_daemon_and_recover_move(&mut fixture.controller);
        assert_eq!(outcome.outcome, "failed", "{kill:?}");
        assert!(
            !outcome.recovery.as_deref().unwrap().contains("Retry Move"),
            "{kill:?}: {:?}",
            outcome.recovery
        );
        let recovered =
            crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
        assert_eq!(recovered.state, SessionState::Error, "{kill:?}");
        let published = recovered.last_error.clone().unwrap();
        assert!(
            published.contains("checkpoint archive is missing"),
            "{kill:?}: {published}"
        );
        assert!(
            !published.contains("checkpoint retained"),
            "{kill:?}: {published}"
        );
        let operation = crate::database::load_move_operation(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        let api = super::ViewerMoveRecovery::from_operation(&operation)
            .unwrap()
            .with_public_failure(Some(&published));
        assert_eq!(
            api.failure_notice.as_deref(),
            Some(published.as_str()),
            "the stored public recovery instructions must reach the conversation"
        );
        assert!(!api.checkpoint_retained, "{kill:?}");
        assert!(api.environment_retained, "{kill:?}");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let refusal = runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                operation.selection.clone(),
                &ProcessExecutor,
            ))
            .unwrap_err();
        assert!(
            format!("{refusal:#}").contains("checkpoint archive is missing"),
            "{kill:?}: {refusal:#}"
        );
        assert_eq!(
            fixture.controller.state.sessions[LATCH_RELAY_SESSION].state,
            SessionState::Error
        );
    }
}

/// A retry that finds its archive gone mid-flight (removed while the daemon
/// runs) records the loss on the Move and leaves the session in `Error`.
// Hard-won: 42de65fd: retry with a missing handoff left the session stuck outside destroyable state
#[test]
#[cfg(unix)]
fn in_place_move_retry_that_finds_its_handoff_gone_leaves_the_session_destroyable() {
    let name =
        test_name("in_place_move_retry_that_finds_its_handoff_gone_leaves_the_session_destroyable");
    if std::env::var_os("MJ_MOVE_RETRY_LOST_HANDOFF_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_RETRY_LOST_HANDOFF_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let source_relay = seed_source_relay(&fixture.worker_root);
    let mut killed = in_place_move_killed_at(DaemonKill::Suspending, &mut fixture, &source_relay);
    // Recovery finishes an uncancelled Move by itself; a cancelled one stops
    // with its environment retained, which is where a manual retry starts.
    killed.cancellation_requested = true;
    crate::database::save_move_operation(&killed).unwrap();
    assert_eq!(
        restart_daemon_and_recover_move(&mut fixture.controller).outcome,
        "cancelled"
    );
    crate::database::clear_move_cancellation_for_retry(LATCH_RELAY_SESSION).unwrap();
    let mut operation = crate::database::load_move_operation(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let preparation =
        runtime
            .block_on(fixture.controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                operation.selection.clone(),
                &ProcessExecutor,
            ))
            .unwrap();
    fs::remove_file(&operation.handoff.as_ref().unwrap().archive_path).unwrap();
    let result = runtime.block_on(async {
        let channels = crate::session_manager::spawn_session_manager().unwrap();
        let result = fixture
            .controller
            .execute_move(
                &crate::controller::test_support::session_admission(
                    &operation.selection.session_id,
                ),
                &mut operation,
                Some(&preparation),
                &recording_executor(),
                &channels.control,
                MoveSourceRelay::default(),
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        result
    });
    let outcome = fixture
        .controller
        .finish_move_result(&mut operation, result, &recording_executor())
        .unwrap();
    assert_eq!(outcome.outcome, "failed");
    let record = crate::database::load_state().unwrap().sessions[LATCH_RELAY_SESSION].clone();
    assert_eq!(
        record.state,
        SessionState::Error,
        "the retry left the session suspending"
    );
    assert!(
        record
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("checkpoint archive is missing")),
        "{:?}",
        record.last_error
    );
    let stored = crate::database::load_move_operation(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    assert!(!stored.checkpoint_retained());
}

#[test]
fn a_refused_sealed_move_retry_names_what_differs_and_what_to_pass() {
    let sealed = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: mj_core::move_workspace::WorkspaceSelection {
            exclusions: Vec::new(),
            acknowledge_large_transfer: true,
        },
        clear_resource_allocation: false,
        session_id: MOVE_QUEUE_SESSION_ID.into(),
        profile_id: Some("fake".into()),
        target_template_id: Some("localhost".into()),
        additional_mounts: Some(Vec::new()),
        resource_allocation: None,
    };
    let mut requested = sealed.clone();
    requested.workspace.acknowledge_large_transfer = false;
    let refusal = super::sealed_selection_difference(&sealed, &requested);
    assert!(refusal.contains("large-transfer"), "{refusal}");
    assert!(refusal.contains("--allow-large-transfer"), "{refusal}");
    requested = sealed.clone();
    requested.oracle = Some(false);
    let refusal = super::sealed_selection_difference(&sealed, &requested);
    assert!(refusal.contains("oracle choice"), "{refusal}");
    requested = sealed.clone();
    requested.oracle_choice = Some(mj_core::config::SubagentChoice {
        model: "advisor".into(),
        effort: None,
    });
    assert!(super::sealed_selection_difference(&sealed, &requested).contains("oracle choice"));
    requested = sealed.clone();
    requested.target_template_id = Some("podman".into());
    let refusal = super::sealed_selection_difference(&sealed, &requested);
    assert!(refusal.contains("--target localhost"), "{refusal}");
    assert!(!refusal.contains("large-transfer"), "{refusal}");
}

/// The size a Move applies where the source and the request leave none, and
/// the retry that repeats the request it sealed (#1585).
#[test]
fn a_move_size_and_a_clear_retry_keep_their_destination_meaning() {
    let config = resume_compatibility_config();
    let mut state = State::default();
    let container_size = |cpus: u64, gib: u64| {
        Some(SessionResourceAllocation::Container {
            cpus,
            memory_bytes: gib << 30,
        })
    };
    let sized_source = |allocation| {
        let mut source = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
        source.resource_allocation = allocation;
        source
    };
    // The same source with an environment of its own, which is what makes a
    // profile-only Move an in-place one.
    let container_source = |allocation| {
        let mut source = sized_source(allocation);
        source.target = Some(Box::new(TargetLocator::LocalPodman {
            container_id: "mj-env-default-container".into(),
            workspace_storage: Default::default(),
            borrowed_from: None,
        }));
        source
    };
    // Record the size the running container actually has, separately from the
    // session's next-creation request.
    let to_applied_size = |source: &mut mj_core::state::SessionRecord, allocation| {
        let mut runtime = mj_core::state::TargetRuntimeSettings::from(&config.targets["podman"]);
        runtime.container_settings = Some(mj_core::state::ContainerSettings {
            resource_allocation: allocation,
            additional_mounts: Vec::new(),
            default_image: None,
        });
        source.target_runtime = Some(runtime.into());
    };
    // A sealed Move of `sealed`, with the container settings acceptance
    // records, so the retry path reads exactly what a real Move stores.
    let prepared_operation = |sealed: &mj_core::state::MoveSelection| {
        let source = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
        let mut operation = source_recovery_operation(&source);
        operation.selection = sealed.clone();
        operation.workspace_transfer = Some(mj_core::move_workspace::WorkspaceTransfer {
            assessment: Default::default(),
            source: Box::new(source.clone()),
            source_stage: "/stage/source".into(),
            controller_stage: "/stage/controller".into(),
            phase: mj_core::move_workspace::WorkspaceTransferPhase::Planned,
        });
        operation.recovery_session = Some(source);
        operation
    };
    let sealed_preparation = |sealed: mj_core::state::MoveSelection, sized_by_default: bool| {
        mj_core::state::MovePreparation {
            destination_checks: Default::default(),
            workspace: None,
            in_place: false,
            stops_subagents: false,
            source_unavailable: false,
            conversion: None,
            selection: sealed,
            sized_by_destination_default: sized_by_default,
            source_profile_id: "codex".into(),
            source_target_template_id: "podman".into(),
            cross_harness: false,
            active: false,
            queued_commands: Vec::new(),
            fingerprint: "sealed".into(),
            operation_id: "move-sealed".into(),
        }
    };
    let selection = |target: &str, allocation, clear| mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        subagents: None,
        workspace: Default::default(),
        clear_resource_allocation: clear,
        session_id: MOVE_QUEUE_SESSION_ID.into(),
        profile_id: Some("codex".into()),
        target_template_id: Some(target.into()),
        additional_mounts: None,
        resource_allocation: allocation,
    };

    // A fixed destination has no default, so the clear request that lets a
    // sized session onto it survives. Resolving it away would make validation
    // inherit the source size and refuse the Move.
    let mut cleared = selection("local-bare", None, true);
    assert!(!super::resolve_move_destination_size(
        &sized_source(container_size(16, 32)),
        &mut cleared,
        &state,
        &config.targets["local-bare"]
    ));
    assert!(cleared.clear_resource_allocation);
    assert_eq!(cleared.resource_allocation, None);

    // A fixed destination has no default, so the clear request survives even
    // though the source has a size: it is what allows the Move onto that host,
    // and validation has to see no allocation to accept it.
    let mut no_default = selection("podman", None, false);
    let fixed_host = mj_core::config::TargetTemplate::LocalBare;
    let mut sizing_source = sized_source(container_size(16, 32));
    assert!(!super::resolve_move_destination_size(
        &sizing_source,
        &mut no_default,
        &state,
        &fixed_host
    ));
    assert_eq!(no_default.resource_allocation, None);
    assert!(!no_default.clear_resource_allocation);
    // An inherited size on a fixed host is left as the request wrote it too.
    sizing_source.resource_allocation = None;
    let mut cleared_on_fixed = selection("local-bare", None, true);
    assert!(!super::resolve_move_destination_size(
        &sizing_source,
        &mut cleared_on_fixed,
        &state,
        &fixed_host
    ));
    assert!(cleared_on_fixed.clear_resource_allocation);
    assert_eq!(cleared_on_fixed.resource_allocation, None);

    // A container destination takes the host's remembered size, else the
    // baseline, and carries it instead of the clear instruction.
    let mut defaulted = selection("podman", None, true);
    assert!(super::resolve_move_destination_size(
        &sized_source(None),
        &mut defaulted,
        &state,
        &config.targets["podman"]
    ));
    assert_eq!(defaulted.resource_allocation, container_size(4, 8));
    assert!(!defaulted.clear_resource_allocation);

    // A size the selection already names is kept, whoever asked for it.
    let mut explicit = selection("podman", container_size(6, 16), false);
    assert!(!super::resolve_move_destination_size(
        &sized_source(None),
        &mut explicit,
        &state,
        &config.targets["podman"]
    ));
    assert_eq!(explicit.resource_allocation, container_size(6, 16));

    // An inherited size becomes the sealed size, so the confirmation and every
    // later reader see the size the rebuilt container really gets instead of
    // working it out from the source record again.
    let mut inheriting = selection("podman", None, false);
    assert!(!super::resolve_move_destination_size(
        &sized_source(container_size(16, 32)),
        &mut inheriting,
        &state,
        &config.targets["podman"]
    ));
    assert_eq!(inheriting.resource_allocation, container_size(16, 32));

    state.remember_container_size(
        "local",
        mj_core::state::HostContainerSize {
            cpus: 12,
            memory_bytes: 24 << 30,
        },
    );
    let mut remembered = selection("podman", None, true);
    super::resolve_move_destination_size(
        &sized_source(None),
        &mut remembered,
        &state,
        &config.targets["podman"],
    );
    assert_eq!(remembered.resource_allocation, container_size(12, 24));

    // A container whose applied size is exactly the default it would clear to
    // keeps its environment. The environment decision and the capacity
    // admission inside execution both read the resolved selection, so they
    // must agree; putting the clear request back would make the second call
    // see no size and refuse the Move as having changed its mind.
    let mut sized_container = sized_source(container_size(12, 24));
    sized_container.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: "mj-env-default-container".into(),
        workspace_storage: Default::default(),
        borrowed_from: None,
    }));
    let mut same_size = selection("podman", None, true);
    assert!(super::resolve_move_destination_size(
        &sized_container,
        &mut same_size,
        &state,
        &config.targets["podman"]
    ));
    assert!(
        super::move_in_place_for_record(
            &sized_container,
            &same_size,
            &config.targets,
            false,
            None,
        )
        .unwrap(),
        "clearing to the size the container already has keeps it"
    );
    // The unresolved request is exactly what the admission call would refuse.
    let mut unresolved = same_size.clone();
    unresolved.resource_allocation = None;
    unresolved.clear_resource_allocation = true;
    assert!(
        !super::move_in_place_for_record(
            &sized_container,
            &unresolved,
            &config.targets,
            false,
            None,
        )
        .unwrap(),
        "the unresolved request would decide the environment changed"
    );

    // Clearing the allocation of a legacy container record does change the
    // environment, because the resolved size is not the one it has.
    let legacy = sized_source(None);
    let mut legacy_cleared = selection("podman", None, true);
    super::resolve_move_destination_size(
        &legacy,
        &mut legacy_cleared,
        &state,
        &config.targets["podman"],
    );
    assert!(
        !super::move_in_place_for_record(&legacy, &legacy_cleared, &config.targets, false, None,)
            .unwrap()
    );

    // A Move sealed by a build that recorded inherited sizing separately
    // stores no allocation at all. Preparation must leave that record exactly
    // as sealed: execution re-prepares it and refuses a selection that
    // differs, so resolving it here would make the retained Move impossible
    // to retry after an upgrade.
    let mut legacy_sealed = selection("podman", None, false);
    let mut retained = prepared_operation(&legacy_sealed);
    retained.in_place = false;
    let mut retried_selection = selection("podman", None, false);
    super::reconcile_retry_resource_selection(&legacy_sealed, false, &mut retried_selection);
    let (kept, sized_by_default) = super::resolve_move_size_for_environment(
        &container_source(container_size(16, 32)),
        &mut retried_selection,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        Some(&retained),
    )
    .unwrap();
    assert!(kept);
    assert!(!sized_by_default);
    assert_eq!(
        retried_selection, legacy_sealed,
        "a sealed selection is not rewritten, so the retry can run"
    );
    // The same Move re-prepared with its own sealed selection keeps matching.
    assert_eq!(retried_selection, retained.selection);
    // A sealed clear request is part of that record too.
    legacy_sealed.clear_resource_allocation = true;
    retained.selection = legacy_sealed.clone();
    let mut cleared_retry = selection("podman", None, true);
    super::reconcile_retry_resource_selection(&legacy_sealed, false, &mut cleared_retry);
    let (kept, sized_by_default) = super::resolve_move_size_for_environment(
        &container_source(container_size(16, 32)),
        &mut cleared_retry,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        Some(&retained),
    )
    .unwrap();
    assert!(
        !kept,
        "clearing the size of a container whose record has one replaces it"
    );
    assert!(!sized_by_default);
    assert_eq!(cleared_retry, retained.selection);
    // Without a retained Move the same request resolves as it always does.
    let mut fresh = selection("podman", None, true);
    assert!(super::resolve_move_destination_size(
        &sized_source(container_size(16, 32)),
        &mut fresh,
        &state,
        &config.targets["podman"]
    ));
    assert_eq!(fresh.resource_allocation, container_size(12, 24));

    // A profile-only Move of a container that records no size changes nothing
    // about the environment, so it stays in place and is given no size.
    // Inventing one would turn it into a rebuild and make the automatic quota
    // switch refuse a session it can currently recover.
    let unsized_container = container_source(None);
    let mut profile_only = selection("podman", None, false);
    let (kept, sized_by_default) = super::resolve_move_size_for_environment(
        &unsized_container,
        &mut profile_only,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        None,
    )
    .unwrap();
    assert!(kept, "an unchanged profile Move keeps the environment");
    assert!(!sized_by_default);
    assert_eq!(profile_only, selection("podman", None, false));

    // The same unsized container asked to clear its sizing is a different
    // request: the person wants the destination's size, so it is resolved and
    // the container is rebuilt with it rather than left unbounded.
    let mut clear_unsized = selection("podman", None, true);
    let (kept, sized_by_default) = super::resolve_move_size_for_environment(
        &unsized_container,
        &mut clear_unsized,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        None,
    )
    .unwrap();
    assert!(!kept, "an unknown size is replaced by a known one");
    assert!(sized_by_default);
    assert_eq!(clear_unsized.resource_allocation, container_size(12, 24));
    assert!(!clear_unsized.clear_resource_allocation);
    assert!(
        !super::move_in_place_for_record(
            &unsized_container,
            &clear_unsized,
            &config.targets,
            false,
            None
        )
        .unwrap()
    );
    // The environment decision the admission inside execution re-makes from
    // the sealed selection agrees with it.
    assert!(
        super::move_in_place_for_record(
            &unsized_container,
            &profile_only,
            &config.targets,
            false,
            None
        )
        .unwrap()
    );
    // A container whose next-creation size was cleared but which is running at
    // the host default: clearing that request is what this state records, and
    // resolving it to the size the container already has keeps the
    // environment. Deciding before resolving would call it a rebuild and then
    // seal a selection that says otherwise, which execution refuses.
    let mut cleared_request = container_source(None);
    to_applied_size(&mut cleared_request, container_size(12, 24));
    let mut defaulted_in_place = selection("podman", None, false);
    let (kept, sized_by_default) = super::resolve_move_size_for_environment(
        &cleared_request,
        &mut defaulted_in_place,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        None,
    )
    .unwrap();
    assert!(
        kept,
        "the size it resolves to is the one the container already has"
    );
    assert!(sized_by_default);
    assert_eq!(
        defaulted_in_place.resource_allocation,
        container_size(12, 24)
    );
    // Execution re-decides from the sealed selection and must agree.
    assert!(
        super::move_in_place_for_record(
            &cleared_request,
            &defaulted_in_place,
            &config.targets,
            false,
            None
        )
        .unwrap()
    );
    // A device default the container does not have is still a resize.
    let mut resized = container_source(None);
    to_applied_size(&mut resized, container_size(4, 8));
    let mut resized_selection = selection("podman", None, false);
    let (kept, _) = super::resolve_move_size_for_environment(
        &resized,
        &mut resized_selection,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        None,
    )
    .unwrap();
    assert!(!kept);
    assert_eq!(
        resized_selection.resource_allocation,
        container_size(12, 24)
    );
    assert!(
        !super::move_in_place_for_record(
            &resized,
            &resized_selection,
            &config.targets,
            false,
            None
        )
        .unwrap()
    );

    // The same profile-only Move that cannot keep its environment is sized for
    // the container it has to create, and stays a rebuild when re-decided.
    let mut provisioned = selection("podman", None, false);
    let mut gone = container_source(None);
    gone.target = None;
    let (kept, sized_by_default) = super::resolve_move_size_for_environment(
        &gone,
        &mut provisioned,
        &state,
        &config.targets,
        &config.targets["podman"],
        false,
        None,
    )
    .unwrap();
    assert!(!kept);
    assert!(sized_by_default);
    assert_eq!(provisioned.resource_allocation, container_size(12, 24));
    assert!(
        !super::move_in_place_for_record(&gone, &provisioned, &config.targets, false, None)
            .unwrap()
    );

    // Retrying the same command after a container Move was sealed carries the
    // sealed default, so the retry is accepted rather than refused as a
    // different selection -- and it does not consult the host's remembered
    // size, which another session may have changed since the Move was sealed.
    let sealed = selection("podman", container_size(12, 24), false);
    let mut retried = selection("podman", None, true);
    super::reconcile_retry_resource_selection(&sealed, true, &mut retried);
    assert_eq!(retried, sealed);
    state.remember_container_size(
        "local",
        mj_core::state::HostContainerSize {
            cpus: 2,
            memory_bytes: 4 << 30,
        },
    );
    let mut retried_after_change = selection("podman", None, true);
    super::reconcile_retry_resource_selection(&sealed, true, &mut retried_after_change);
    assert_eq!(
        retried_after_change, sealed,
        "the sealed Move owns the size it resolved, not the host's current one"
    );

    // A sealed size that was not this Move's default stays a difference: the
    // person asked for the default and the Move would apply something else.
    let mut conflicting = selection("podman", None, true);
    super::reconcile_retry_resource_selection(&sealed, false, &mut conflicting);
    assert!(conflicting.clear_resource_allocation);
    assert_ne!(conflicting, sealed);

    // Acceptance prepares the already-resolved selection again, so that pass
    // cannot resolve anything itself. The provenance comes from the sealed
    // Move, which is what keeps the repeated clear request recognisable.
    let mut accepted = prepared_operation(&sealed);
    accepted.accepted_preparation = Some(Box::new(sealed_preparation(sealed.clone(), true)));
    assert!(
        super::move_size_is_destination_default(false, Some(&accepted)),
        "a re-preparation inherits the decision the Move was sealed with"
    );
    let mut retried_after_acceptance = selection("podman", None, true);
    super::reconcile_retry_resource_selection(&sealed, true, &mut retried_after_acceptance);
    assert_eq!(retried_after_acceptance, sealed);
    // Without that record nothing claims the size came from clearing.
    let mut unproven = prepared_operation(&sealed);
    unproven.accepted_preparation = Some(Box::new(sealed_preparation(sealed.clone(), false)));
    assert!(!super::move_size_is_destination_default(
        false,
        Some(&unproven)
    ));
    // A Move that no longer holds its environment is not a retry at all.
    let mut released = prepared_operation(&sealed);
    released.workspace_transfer = None;
    released.recovery_session = None;
    released.accepted_preparation = Some(Box::new(sealed_preparation(sealed.clone(), true)));
    assert!(!super::move_size_is_destination_default(
        false,
        Some(&released)
    ));

    // A retry that says nothing keeps what the Move was sealed with, and a
    // fixed destination's sealed clear is the same request.
    let mut omitted = selection("podman", None, false);
    super::reconcile_retry_resource_selection(&sealed, false, &mut omitted);
    assert_eq!(omitted, sealed);
    let sealed_fixed = selection("local-bare", None, true);
    let mut retried_fixed = selection("local-bare", None, true);
    super::reconcile_retry_resource_selection(&sealed_fixed, false, &mut retried_fixed);
    assert_eq!(retried_fixed, sealed_fixed);
}

#[test]
fn move_queue_handoff_never_exposes_unowned_session() {
    let session = "move-ownership-concurrent-handoff";
    let guard = MoveMutationGuard::reserve(session).unwrap();
    let reader = std::thread::spawn(move || {
        for _ in 0..10_000 {
            assert!(move_owns_session(session));
        }
    });
    let mut guard = Some(guard);
    for _ in 0..1000 {
        super::set_move_queue_hold(session, true);
        assert!(super::move_has_pending_queue(session));
        drop(guard.take());
        assert!(move_owns_session(session));
        guard = Some(MoveMutationGuard::reserve(session).unwrap());
        super::release_move_queue_hold(session);
        assert!(!super::move_has_pending_queue(session));
    }
    reader.join().unwrap();
    drop(guard);
    assert!(!move_owns_session(session));
}

#[cfg(unix)]
#[test]
fn in_place_move_with_oversized_file_exports_only_session_handoff() {
    let name = test_name("in_place_move_with_oversized_file_exports_only_session_handoff");
    if std::env::var_os("MJ_MOVE_LARGE_IN_PLACE_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_MOVE_LARGE_IN_PLACE_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Codex, HarnessKind::Codex);
    fs::File::create(fixture.checkout.join("large-build-output"))
        .unwrap()
        .set_len(9 * 1024 * 1024 * 1024)
        .unwrap();
    fixture
        .controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap()
        .checkpoint = None;
    crate::database::save_session(&fixture.controller.state.sessions[LATCH_RELAY_SESSION]).unwrap();
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    crate::database::save_move_operation(&operation).unwrap();
    let executor = recording_executor();
    run_in_place_move(
        &mut fixture.controller,
        &mut operation,
        &fixture.worker_root,
        &executor,
    )
    .unwrap();
    assert!(fixture.checkout.join("large-build-output").exists());
    let handoff = operation.handoff.as_ref().unwrap();
    assert!(fs::metadata(&handoff.archive_path).unwrap().len() < 1024 * 1024);
    let verified = mj_checkpoint::archive::verify_archive_streaming(&handoff.archive_path).unwrap();
    assert!(
        verified
            .manifest
            .payloads
            .iter()
            .filter(|payload| payload.path.starts_with("repositories/"))
            .all(|payload| payload.size < 1024)
    );
    assert!(
        fixture.controller.state.sessions[LATCH_RELAY_SESSION]
            .checkpoint
            .is_none()
    );
}

/// Restart uses the same bounded handoff as Move, even with a workspace too
/// large to checkpoint, and keeps both the staged native state and full copy.
#[cfg(unix)]
#[test]
fn restart_with_oversized_workspace_retains_native_state_and_full_checkpoint() {
    use std::os::unix::fs::MetadataExt;

    let name =
        test_name("restart_with_oversized_workspace_retains_native_state_and_full_checkpoint");
    if std::env::var_os("MJ_RESTART_HANDOFF_CHILD").is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(name)
            .env("MJ_RESTART_HANDOFF_CHILD", "1")
            .env(LATCH_CHECKPOINT_ONLY, "1")
            .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
            .isolated_store(directory.path())
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Codex, HarnessKind::Codex);
    let large_file = fixture.checkout.join("large-build-output");
    fs::File::create(&large_file)
        .unwrap()
        .set_len(9 * 1024 * 1024 * 1024)
        .unwrap();
    let checkout_inode = fs::metadata(&fixture.checkout).unwrap().ino();
    let native_file = fixture.worker_root.join("profile/source-only.txt");
    let native_inode = fs::metadata(&native_file).unwrap().ino();
    let source_target = fixture.controller.state.sessions[LATCH_RELAY_SESSION]
        .target
        .clone();
    let intent = crate::database::begin_session_restart(
        LATCH_RELAY_SESSION,
        "restart-handoff",
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        false,
    )
    .unwrap();
    crate::database::mark_session_restart_checkpoint_started(
        LATCH_RELAY_SESSION,
        &intent.operation_id,
    )
    .unwrap();
    let source_relay = seed_source_relay(&fixture.worker_root);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = recording_executor();
    runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let result = fixture
            .controller
            .suspend_session_for_restart(
                &crate::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                LATCH_RELAY_SESSION,
                &executor,
                &channels.control,
                None,
                false,
            )
            .await;
        channels.shutdown.shutdown().await.unwrap();
        result.unwrap();
    });
    let intent = crate::database::load_session_restart(LATCH_RELAY_SESSION)
        .unwrap()
        .unwrap();
    let handoff = intent.handoff.as_ref().expect("Restart owns a handoff");
    assert_eq!(
        intent.checkpoint_sha256.as_deref(),
        Some(handoff.sha256.as_str())
    );
    assert!(fs::metadata(&handoff.archive_path).unwrap().len() < 1024 * 1024);
    let verified = mj_checkpoint::archive::verify_archive_streaming(&handoff.archive_path).unwrap();
    assert!(
        verified
            .manifest
            .payloads
            .iter()
            .filter(|payload| payload.path.starts_with("repositories/"))
            .all(|payload| payload.size < 1024)
    );
    assert_eq!(
        fixture.controller.state.sessions[LATCH_RELAY_SESSION]
            .checkpoint
            .as_deref(),
        Some(&fixture.checkpoint)
    );
    // Model startup: reload durable ownership and sweep unreferenced archives.
    crate::controller::checkpoint::reconcile_managed_checkpoint_archives().unwrap();
    assert!(handoff.archive_path.exists());
    crate::database::advance_session_restart(
        LATCH_RELAY_SESSION,
        &intent.operation_id,
        crate::database::SessionRestartPhase::RestoringInPlace,
    )
    .unwrap();
    runtime
        .block_on(crate::controller::test_support::worker_test_scope_async(
            LATCH_RELAY_SESSION,
            fixture.controller.restore_session_in_place_for_restart(
                LATCH_RELAY_SESSION,
                IN_PLACE_SOURCE_PROFILE,
                "local-bare",
                &executor,
            ),
        ))
        .unwrap()
        .unwrap();
    let record = &fixture.controller.state.sessions[LATCH_RELAY_SESSION];
    assert_eq!(record.state, SessionState::Running);
    assert_eq!(record.target, source_target);
    assert_eq!(record.checkpoint.as_deref(), Some(&fixture.checkpoint));
    assert!(fixture.checkpoint.archive_path.exists());
    assert_eq!(
        fs::metadata(&fixture.checkout).unwrap().ino(),
        checkout_inode
    );
    assert_eq!(fs::metadata(&native_file).unwrap().ino(), native_inode);
    assert_eq!(fs::read(native_file).unwrap(), b"old harness state");
    assert_eq!(
        fs::metadata(large_file).unwrap().len(),
        9 * 1024 * 1024 * 1024
    );
    crate::database::finish_session_restart(LATCH_RELAY_SESSION, &intent.operation_id).unwrap();
    crate::controller::checkpoint::reconcile_managed_checkpoint_archives().unwrap();
    assert!(!handoff.archive_path.exists());
    assert!(fixture.checkpoint.archive_path.exists());
}

#[cfg(unix)]
struct Ec2PreviewExecutor(GitWithPodmanPreflightExecutor);

#[cfg(unix)]
impl CommandExecutor for Ec2PreviewExecutor {
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
        if command.program == "aws" {
            assert!(
                !command.args.iter().any(|a| a == "run-instances"),
                "preview must not launch EC2"
            );
            return Ok(CommandOutput {
                status: 0,
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            });
        }
        self.0.execute(command)
    }
    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn io::Read + Send),
    ) -> Result<CommandOutput> {
        self.0.execute_with_stdin(command, input)
    }
}

#[cfg(unix)]
#[test]
fn retired_ec2_move_is_rejected_before_source_interruption() {
    let short = "retired_ec2_move_is_rejected_before_source_interruption";
    if !isolated_test_child(&test_name(short), "MJ_MOVE_EC2_PREVIEW_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let repository = committed_repository();
    let (_remote_parent, remote) =
        crate::controller::test_support::network_remote_for(repository.path());
    fs::write(
        repository.path().join("selected-untracked.txt"),
        vec![b'x'; 200 * 1024],
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut config = resume_compatibility_config();
    add_codex_profile(&mut config, home.path());
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));
    config
        .targets
        .insert("ec2".into(), serde_json::from_value(serde_json::json!({
            "kind": "aws-ec2", "region": "us-east-1", "launch_template": "lt-old", "ssh_user": "ubuntu"
        })).unwrap());
    let id = "0123456789abcdef0123456789abcdef";
    let mut session = raw_session_on("local-bare", &repository.path().to_string_lossy());
    session.bundle_id = "project".into();
    session.state = SessionState::Running;
    session.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: mj_core::config::data_dir().join("workers").join(id),
    }));
    session.resource_allocation = Some(SessionResourceAllocation::Container {
        cpus: 2,
        memory_bytes: 4 * 1024 * 1024 * 1024,
    });
    let state = State {
        sessions: [(id.into(), session)].into_iter().collect(),
        ..Default::default()
    };
    crate::database::save_state(&state).unwrap();
    let controller = Controller { config, state };
    let selection = mj_core::state::MoveSelection {
        oracle: None,
        oracle_choice: None,
        session_id: id.into(),
        target_template_id: Some("ec2".into()),
        profile_id: Some("codex".into()),
        additional_mounts: None,
        resource_allocation: None,
        clear_resource_allocation: false,
        workspace: Default::default(),
        subagents: None,
    };
    let executor = Ec2PreviewExecutor(GitWithPodmanPreflightExecutor { remote });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let before = controller.state.sessions[id].clone();
    let error =
        runtime
            .block_on(controller.prepare_move_session_controlled(
                &crate::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                selection,
                &executor,
            ))
            .unwrap_err();
    assert!(
        format!("{error:#}").contains(mj_core::config::LEGACY_EC2_TARGET_NOTICE),
        "{error:#}"
    );
    assert_eq!(
        crate::database::load_session_record(id).unwrap().unwrap(),
        before
    );
    assert!(crate::database::load_move_operation(id).unwrap().is_none());
}

/// The executor the move tests use, running mj-worker's checkpoint operations.
#[cfg(unix)]
fn recording_executor() -> RecordingProcessExecutor {
    RecordingProcessExecutor::new(WorkerCheckpoint {
        restore: mj_worker::checkpoint::restore_from_spec_file,
        export: |mut input| {
            let checkpoint = mj_worker::checkpoint::export_from_spec_reader(&mut input)?;
            Ok(serde_json::to_vec(&checkpoint)?)
        },
    })
}

#[cfg(unix)]
// Hard-won: a70ead9: orphan recovery must use one live cut, including native staging and queue semantics.
#[test]
fn adoption_installs_a_coherent_live_baseline_and_reattaches_normally() {
    use mj_core::native_agent::{NativeAgentCapabilities, NativeAgentEvent};
    use mj_core::relay::{RelayObservation, RelayRequestEnvelope, RelayResponseBody};
    let name = test_name("adoption_installs_a_coherent_live_baseline_and_reattaches_normally");
    if !isolated_test_child(&name, "MJ_LIVE_BASELINE_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    let _proxy_guard =
        install_move_queue_relay_worker(&root, &directory.path().join("marker"), true);
    let mut image_bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    image_bytes.extend(vec![0; 70 * 1024]);
    let image =
        mj_core::attachment::AttachmentRef::new(&image_bytes, "image/png".into(), 1, 1).unwrap();
    mj_core::attachment::AttachmentStore::worker(&root)
        .install(&image, &image_bytes)
        .unwrap();
    // Hard-won: cc8c246: recovered startup images need their own cache transfer,
    // independently of attachments still present in the current queue.
    let mut startup_image_bytes = image_bytes.clone();
    *startup_image_bytes.last_mut().unwrap() = 1;
    let startup_image =
        mj_core::attachment::AttachmentRef::new(&startup_image_bytes, "image/png".into(), 1, 1)
            .unwrap();
    mj_core::attachment::AttachmentStore::worker(&root)
        .install(&startup_image, &startup_image_bytes)
        .unwrap();
    let initial = vec![mj_core::worker_launch::InitialWorkerCommand {
        command_id: "startup-image-brief".into(),
        optional: false,
        command: RelayCommand::Prompt {
            prompt: vec![
                ContentBlock::from("original image task"),
                startup_image.content_block(),
            ],
        },
    }];
    fs::write(
        root.join("test-startup-commands.json"),
        serde_json::to_vec(&initial).unwrap(),
    )
    .unwrap();
    let mut relay = DurableRelay::open_for_harness(
        &root,
        MOVE_QUEUE_SESSION_ID,
        "test",
        HarnessKind::Codex,
        &initial,
    )
    .unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "surviving-native".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(
            "startup-image-brief",
            mj_core::relay::RelayCommandOutcome::Prompt {
                stop_reason: "end_turn".into(),
                diagnostic: None,
                usage: None,
                work_facts: None,
            },
        )
        .unwrap();
    let stale = mj_core::elicitation::ElicitationRequest::from_acp_params("stale-question", serde_json::json!({
        "mode":"form", "sessionId":MOVE_QUEUE_SESSION_ID, "message":"Old runtime", "requestedSchema":{"type":"object","properties":{}}
    })).unwrap();
    relay
        .record_observation(RelayObservation::ElicitationRequested { request: stale })
        .unwrap();
    drop(relay);
    let mut relay = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "reopened").unwrap();
    relay
        .record_observation(RelayObservation::SessionRestarted)
        .unwrap();
    assert!(relay.live_session_baseline().unwrap().questions.is_empty());
    assert_eq!(
        relay.live_session_baseline().unwrap().unavailable_questions,
        0
    );
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "surviving-native".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    let submit = |relay: &mut DurableRelay, id: &str, command| {
        let response = relay.handle(RelayRequestEnvelope {
            request_id: id.into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Submit {
                command_id: id.into(),
                command,
            },
        });
        assert!(
            matches!(response.body, RelayResponseBody::Ok { .. }),
            "{:?}",
            response.body
        );
    };
    submit(
        &mut relay,
        "accepted-model",
        RelayCommand::SetConfig {
            key: "model".into(),
            value: "chosen-picker".into(),
        },
    );
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(
            "accepted-model",
            mj_core::relay::RelayCommandOutcome::Configured,
        )
        .unwrap();
    submit(
        &mut relay,
        "active-prompt",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("active work")],
        },
    );
    relay.claim_pending_commands(true).unwrap();
    submit(
        &mut relay,
        "steered-before-cut",
        RelayCommand::Prompt {
            prompt: vec![ContentBlock::from("old steer")],
        },
    );
    // Keep an uncertain steering delivery queued across the idle cut.
    submit(
        &mut relay,
        "old-steer-control",
        RelayCommand::Steer {
            active_prompt_id: "active-prompt".into(),
            queued_prompt_id: "steered-before-cut".into(),
        },
    );
    relay
        .record_observation(RelayObservation::SteeringUnconfirmed {
            command_id: "old-steer-control".into(),
            message: "delivery unknown".into(),
        })
        .unwrap();
    submit(
        &mut relay,
        "queued-prompt",
        RelayCommand::Prompt {
            prompt: vec![
                ContentBlock::from("queued payload".repeat(6000)),
                image.content_block(),
            ],
        },
    );
    submit(
        &mut relay,
        "queued-config",
        RelayCommand::SetConfig {
            key: "effort".into(),
            value: "high".into(),
        },
    );
    let mail = mj_core::mailbox::MailboxEvent {
        key: "recovery-mail".into(),
        source: "operator".into(),
        wake: true,
        created_at_ms: 1,
        body: mj_core::mailbox::MailboxEventBody::OperatorBack {
            requests: vec!["pending work".into()],
        },
    };
    submit(
        &mut relay,
        "deliver-mail",
        RelayCommand::DeliverMailboxEvent {
            event: mail.clone(),
        },
    );
    submit(
        &mut relay,
        "queued-mailbox",
        RelayCommand::MailboxWake { events: vec![mail] },
    );
    let question = mj_core::elicitation::ElicitationRequest::from_acp_params("pending-question", serde_json::json!({
        "mode":"form", "sessionId":MOVE_QUEUE_SESSION_ID, "message":"Approve?", "requestedSchema":{"type":"object", "properties":{}}
    })).unwrap();
    relay
        .record_observation(RelayObservation::ElicitationRequested {
            request: question.clone(),
        })
        .unwrap();
    relay.record_observation(RelayObservation::SessionConfigured { config_options: vec![serde_json::from_value(serde_json::json!({
        "id":"mode", "name":"Mode", "type":"select", "currentValue":"old", "options":[{"value":"old","name":"Old"},{"value":"new","name":"New"}]
    })).unwrap(), serde_json::from_value(serde_json::json!({
        "id":"model", "name":"Model", "type":"select", "currentValue":"unselectable-transcript-id", "options":[{"value":"chosen-picker","name":"Chosen"}]
    })).unwrap()] }).unwrap();
    relay
        .record_observation(RelayObservation::SessionModesConfigured {
            modes: Some(
                serde_json::from_value(serde_json::json!({
                    "currentModeId":"legacy", "availableModes":[{"id":"legacy","name":"Legacy"}]
                }))
                .unwrap(),
            ),
        })
        .unwrap();
    relay
        .record_session_update(
            serde_json::from_value(
                serde_json::json!({"sessionUpdate":"current_mode_update", "currentModeId":"new"}),
            )
            .unwrap(),
        )
        .unwrap();
    relay
        .record_observation(RelayObservation::NativeAgent {
            event: NativeAgentEvent::ReplayBegin,
        })
        .unwrap();
    relay
        .record_observation(RelayObservation::NativeAgent {
            event: NativeAgentEvent::Spawned {
                session_id: "staged-child".into(),
                parent_session_id: None,
                name: "child".into(),
                task: "work".into(),
                capabilities: NativeAgentCapabilities::default(),
            },
        })
        .unwrap();
    relay
        .record_command_completed(
            "active-prompt",
            mj_core::relay::RelayCommandOutcome::Prompt {
                stop_reason: "end_turn".into(),
                diagnostic: None,
                usage: None,
                work_facts: None,
            },
        )
        .unwrap();
    let cut = relay.live_session_baseline().unwrap();
    assert!(cut.native_use_evidence.is_some());
    let frontier = cut.state.latest_ordinal;
    let digest = cut.state.latest_digest.clone();
    relay.handle(RelayRequestEnvelope {
        request_id: "ack".into(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Acknowledge {
            through_ordinal: frontier,
            through_digest: digest.clone(),
        },
    });
    drop(relay);
    crate::database::save_session(&checkpoint_test_session(MOVE_QUEUE_SESSION_ID)).unwrap();
    let spec = CommandSpec::new(root.join("hel").to_str().unwrap(), Vec::<String>::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut connection = crate::session_manager::StandaloneSession::connect_adopting(
            &spec,
            MOVE_QUEUE_SESSION_ID,
        )
        .await
        .unwrap();
        let projection = connection.snapshot().materialized;
        let notice = projection
            .transcript
            .iter()
            .find(|item| {
                item.stable_id == mj_core::transcript::ADOPTION_HISTORY_UNAVAILABLE_ITEM_ID
            })
            .unwrap();
        assert_eq!(notice.position, frontier);
        assert!(
            serde_json::to_string(notice)
                .unwrap()
                .contains("History before recovery unavailable")
        );
        assert_eq!(projection.applied_event_ordinal, frontier + 3);
        assert!(
            mj_core::attachment::AttachmentStore::controller(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .contains(&image)
                .unwrap()
        );
        let queued = projection
            .queued_prompts
            .iter()
            .find(|q| q.command_id == "queued-prompt")
            .unwrap();
        assert_eq!(queued.content[0]["text"], "queued payload".repeat(6000));
        assert_eq!(queued.content.len(), 2);
        assert_eq!(projection.queued_prompts.len(), 4);
        assert!(
            projection
                .queued_prompts
                .iter()
                .find(|q| q.command_id == "queued-mailbox")
                .unwrap()
                .content
                .is_empty()
        );
        assert!(
            projection
                .transcript
                .iter()
                .any(|item| item.stable_id == "user:startup-image-brief")
        );
        assert!(
            mj_core::attachment::AttachmentStore::controller(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .contains(&startup_image)
                .unwrap()
        );
        assert_eq!(projection.pending_elicitations, vec![question]);
        assert_eq!(projection.configuration.values["mode"], "new");
        assert_eq!(projection.configuration.values["model"], "chosen-picker");
        assert!(
            projection.active_turn.is_none(),
            "pre-cut steering identity is intentionally unknown"
        );
        assert_eq!(
            projection
                .transcript
                .iter()
                .filter(|item| matches!(
                    item.body,
                    mj_core::transcript::TranscriptBody::Agent { .. }
                ))
                .count(),
            1
        );
        let before = serde_json::to_string(&projection.transcript).unwrap();
        assert!(before.contains("after recovery cut"));
        connection.sync().await.unwrap();
        assert_eq!(
            serde_json::to_string(&connection.snapshot().materialized.transcript).unwrap(),
            before
        );
    });
    let child = crate::database::load_native_agent_view(MOVE_QUEUE_SESSION_ID, "staged-child", 100)
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_string(&child)
            .unwrap()
            .contains("child after recovery cut")
    );
}

#[cfg(unix)]
// Hard-won: c07fe902: intact genesis was discarded and old workers were refused unnecessarily.
#[test]
fn adoption_reattaches_from_genesis_before_requiring_a_live_baseline() {
    use mj_core::relay::{RelayObservation, RelayRequestEnvelope};
    let name = test_name("adoption_reattaches_from_genesis_before_requiring_a_live_baseline");
    if !isolated_test_child(&name, "MJ_ADOPT_INTACT_GENESIS_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    let _proxy_guard =
        install_move_queue_relay_worker(&root, &directory.path().join("marker"), true);
    fs::write(root.join("protocol-version"), "71").unwrap();
    fs::write(
        root.join("test-native-ready"),
        b"the original harness is still running",
    )
    .unwrap();
    let mut relay = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "legacy").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "legacy-native".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    let mut backup = mj_core::state::MaterializedSession::empty(MOVE_QUEUE_SESSION_ID);
    backup.applied_event_ordinal = relay.latest_ordinal();
    backup.applied_event_digest = relay.latest_digest().to_owned();
    relay.handle(RelayRequestEnvelope {
        request_id: "prompt".into(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "legacy-prompt".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("preserved request")],
            },
        },
    });
    relay.record_session_update(serde_json::from_value(serde_json::json!({
        "sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"preserved historical reply"}
    })).unwrap()).unwrap();
    let frontier = relay.latest_ordinal();
    drop(relay);
    crate::database::save_session(&checkpoint_test_session(MOVE_QUEUE_SESSION_ID)).unwrap();
    let spec = CommandSpec::new(root.join("hel").to_str().unwrap(), Vec::<String>::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let connection = crate::session_manager::StandaloneSession::connect_adopting(
            &spec,
            MOVE_QUEUE_SESSION_ID,
        )
        .await
        .unwrap();
        let projection = connection.snapshot().materialized;
        assert_eq!(projection.applied_event_ordinal, frontier);
        assert!(
            serde_json::to_string(&projection.transcript)
                .unwrap()
                .contains("preserved historical reply")
        );
        assert!(!projection.transcript.iter().any(
            |item| item.stable_id == mj_core::transcript::ADOPTION_HISTORY_UNAVAILABLE_ITEM_ID
        ));
        assert!(!root.join("baseline-frontier").exists());
    });
    // Hard-won: cc8c246: the record may lag the already committed/ACKed projection.
    let mut provisional = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    provisional.state = SessionState::Disconnected;
    provisional.native_session_id = None;
    provisional.target_template_id = "local-bare".into();
    provisional.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.clone(),
    }));
    crate::database::save_session(&provisional).unwrap();
    let mut retry = Controller {
        config: Config::default(),
        state: State::default(),
    };
    retry.config.targets.insert(
        "local-bare".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    retry
        .state
        .sessions
        .insert(MOVE_QUEUE_SESSION_ID.into(), provisional);
    runtime.block_on(async {
        retry
            .adopt_orphan_worker(
                MOVE_QUEUE_SESSION_ID,
                "local-bare",
                None,
                None,
                false,
                &ProcessExecutor,
            )
            .await
            .unwrap();
    });
    assert_eq!(
        retry.state.sessions[MOVE_QUEUE_SESSION_ID].state,
        SessionState::Running
    );
    assert!(retry.state.sessions[MOVE_QUEUE_SESSION_ID].target.is_some());
    assert!(
        serde_json::to_string(
            &crate::database::load_materialized_session(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .unwrap()
                .transcript
        )
        .unwrap()
        .contains("preserved historical reply")
    );
    // A tracked projection restored behind the ACK must fail visibly and
    // remain intact; its explicit recovery is a separately approved follow-up.
    crate::database::save_materialized_session(&backup).unwrap();
    runtime.block_on(async {
        let error = crate::session_manager::StandaloneSession::connect_command(
            &spec,
            MOVE_QUEUE_SESSION_ID,
        )
        .await
        .err()
        .expect("old tracked projection cannot replay a pruned cursor");
        assert!(
            format!("{error:#}")
                .contains("controller store is behind the worker's retained journal")
        );
    });
    assert_eq!(
        crate::database::materialized_event_frontier(MOVE_QUEUE_SESSION_ID).unwrap(),
        Some((backup.applied_event_ordinal, backup.applied_event_digest))
    );
    // Model an old worker whose checkpoint floor already removed genesis. The
    // current relay stand-in collected that prefix when the first attach ACKed.
    crate::database::save_materialized_session(&mj_core::state::MaterializedSession::empty(
        MOVE_QUEUE_SESSION_ID,
    ))
    .unwrap();
    runtime.block_on(async {
        let error = crate::session_manager::StandaloneSession::connect_adopting(
            &spec,
            MOVE_QUEUE_SESSION_ID,
        )
        .await
        .err()
        .expect("trimmed legacy worker needs baseline support");
        assert!(
            format!("{error:#}").contains("restart that worker on the current binary, then adopt")
        );
    });
    let mut record = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    record.state = SessionState::Disconnected;
    record.native_session_id = None;
    record.target_template_id = "local-bare".into();
    record.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.clone(),
    }));
    crate::database::save_session(&record).unwrap();
    let mut state = State::default();
    state.sessions.insert(MOVE_QUEUE_SESSION_ID.into(), record);
    let mut config = Config::default();
    config.targets.insert(
        "local-bare".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    let mut controller = Controller { config, state };
    runtime.block_on(async {
        let error = controller.adopt_orphan_worker(MOVE_QUEUE_SESSION_ID, "local-bare", None, None, false, &ProcessExecutor).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("history no longer reaches its start"), "{text}");
        assert!(text.contains("predates live-cut recovery"), "{text}");
        assert!(text.contains("agent's work continues on the worker"), "{text}");
        assert!(text.contains("harness session files remain on the target"), "{text}");
        assert!(text.contains(&format!("mj recover destroy --session '{MOVE_QUEUE_SESSION_ID}' --target 'local-bare' --confirm '{MOVE_QUEUE_SESSION_ID}'")), "{text}");
        assert!(!text.contains("restart"), "orphan refusal must not offer an unreachable restart: {text}");
    });
    assert!(!root.join("baseline-frontier").exists());
}

#[cfg(unix)]
// Hard-won: cdb0af1: capability refusal claimed the orphan and advertised an unusable destroy command.
#[test]
fn adoption_refusal_keeps_a_foreign_orphan_scannable_and_destroyable() {
    use mj_core::relay::{RelayObservation, RelayRequestEnvelope};
    const MARKER: &str = "MJ_ADOPT_FOREIGN_ORPHAN_CHILD";
    let name = test_name("adoption_refusal_keeps_a_foreign_orphan_scannable_and_destroyable");
    if std::env::var_os(MARKER).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worker");
        let _proxy_guard =
            install_move_queue_relay_worker(&root, &directory.path().join("served"), true);
        fs::write(root.join("protocol-version"), "71").unwrap();
        let listing = serde_json::to_string(&serde_json::json!([{"Labels":{
            "dev.mj.managed":"true", "dev.mj.session":MOVE_QUEUE_SESSION_ID,
            "dev.mj.instance":"foreign-instance"
        }}]))
        .unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        install_fake_command(
            &bin,
            "podman",
            &format!(
                r#"#!/bin/sh
case "$1" in
ps) if [ -e {stopped} ]; then printf '[]'; else printf '%s' {listing}; fi ;;
exec) case "$*" in
  *'worker proxy'*) exec {relay} ;;
  *'worker stop'*) touch {stopped} ;;
  *ownership.json*|*'test -d'*) exit 1 ;;
  *) exit 0 ;;
esac ;;
container) case "$2" in
  inspect) [ ! -e {removed} ] || exit 1
    case "$*" in
      *Config.Labels*) printf 'true|{session}' ;;
      *State.Running*) if [ -e {stopped} ]; then printf false; else printf true; fi ;;
      *) printf '[]' ;;
    esac ;;
  exists) [ ! -e {removed} ] ;;
  *) exit 1 ;;
esac ;;
stop) touch {stopped} ;;
rm) touch {stopped} {removed} ;;
info) exit 0 ;;
*) exit 1 ;;
esac
"#,
                stopped = shell_literal(&root.join("stopped")),
                removed = shell_literal(&root.join("removed")),
                session = MOVE_QUEUE_SESSION_ID,
                listing = mj_core::targets::posix_quote(&listing),
                relay = shell_literal(&root.join("hel"))
            ),
        );
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        IsolatedTest::new(name)
            .isolated_store(directory.path())
            .env(MARKER, &root)
            .env("PATH", path)
            .env("MJ_INSTANCE", "orphan-recovery-test")
            .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let root = PathBuf::from(std::env::var_os(MARKER).unwrap());
    let mut relay = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "legacy").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "legacy-native".into(),
            resumed: true,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
        })
        .unwrap();
    let through = mj_core::relay::RelayCursor {
        ordinal: relay.latest_ordinal(),
        digest: relay.latest_digest().to_owned(),
    };
    relay.handle(RelayRequestEnvelope {
        request_id: "checkpoint-floor".into(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "checkpoint-floor".into(),
            command: RelayCommand::AdvanceRecoveryFloor { through },
        },
    });
    relay.handle(RelayRequestEnvelope {
        request_id: "prune-legacy".into(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Acknowledge {
            through_ordinal: relay.latest_ordinal(),
            through_digest: relay.latest_digest().to_owned(),
        },
    });
    drop(relay);
    let repository = committed_repository();
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    add_codex_profile(&mut config, home.path());
    config
        .bundles
        .insert("project".into(), local_bundle(repository.path()));
    config.targets.insert(
        "old-target".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: mj_core::config::ContainerTemplate {
                image: "ignored".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
                build_cache: None,
            },
        },
    );
    let mut controller = Controller {
        config,
        state: State::default(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let refuse = |controller: &mut Controller| {
        runtime.block_on(async {
            controller
                .adopt_orphan_worker(
                    MOVE_QUEUE_SESSION_ID,
                    "old-target",
                    Some("codex"),
                    Some("project"),
                    true,
                    &ProcessExecutor,
                )
                .await
                .unwrap_err()
        })
    };
    let error = refuse(&mut controller);
    let expected = format!(
        "mj --instance 'orphan-recovery-test' recover destroy --session '{MOVE_QUEUE_SESSION_ID}' --target 'old-target' --confirm '{MOVE_QUEUE_SESSION_ID}' --all-instances"
    );
    assert!(format!("{error:#}").ends_with(&expected), "{error:#}");
    assert!(
        !controller
            .state
            .sessions
            .contains_key(MOVE_QUEUE_SESSION_ID)
    );
    assert!(
        crate::database::load_session_record(MOVE_QUEUE_SESSION_ID)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        controller
            .scan_orphan_workers(&ProcessExecutor, true)
            .candidates
            .len(),
        1
    );
    // An earlier version may already have saved its provisional locator. Keep
    // its history and diagnostic, release the resource, and make normal destroy legal.
    let mut provisional = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    provisional.state = SessionState::Disconnected;
    provisional.native_session_id = None;
    provisional.target_template_id = "old-target".into();
    provisional.target = Some(Box::new(TargetLocator::LocalPodman {
        container_id: crate::targets::resource_name(MOVE_QUEUE_SESSION_ID).unwrap(),
        workspace_storage: mj_core::state::PodmanWorkspaceLocator::ContainerLayer,
        borrowed_from: None,
    }));
    provisional.last_error = Some("earlier adoption diagnostic".into());
    crate::database::save_session(&provisional).unwrap();
    controller
        .state
        .sessions
        .insert(MOVE_QUEUE_SESSION_ID.into(), provisional);
    let mut saved = mj_core::state::MaterializedSession::empty(MOVE_QUEUE_SESSION_ID);
    saved
        .configuration
        .values
        .insert("retained-history-proof".into(), serde_json::json!(true));
    crate::database::save_materialized_session(&saved).unwrap();
    let error = refuse(&mut controller);
    assert!(format!("{error:#}").ends_with(&expected), "{error:#}");
    let leftover = &controller.state.sessions[MOVE_QUEUE_SESSION_ID];
    assert_eq!(leftover.state, SessionState::Lost);
    assert!(leftover.target.is_none());
    assert!(
        leftover
            .last_error
            .as_deref()
            .unwrap()
            .contains("earlier adoption diagnostic")
    );
    assert!(
        crate::database::load_materialized_session(MOVE_QUEUE_SESSION_ID)
            .unwrap()
            .unwrap()
            .configuration
            .values["retained-history-proof"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        controller
            .scan_orphan_workers(&ProcessExecutor, true)
            .candidates
            .len(),
        1
    );
    // The printed command's instance and cross-instance scope match this exact
    // recovery target; its destroy operation must now reach the orphan.
    assert_eq!(
        mj_core::config::instance_name().as_deref(),
        Some("orphan-recovery-test")
    );
    crate::controller::test_support::worker_test_scope(MOVE_QUEUE_SESSION_ID, || {
        controller.destroy_orphan_worker(
            MOVE_QUEUE_SESSION_ID,
            "old-target",
            MOVE_QUEUE_SESSION_ID,
            true,
            &ProcessExecutor,
        )
    })
    .unwrap();
    crate::controller::test_support::worker_test_scope(MOVE_QUEUE_SESSION_ID, || {
        controller.destroy_session_controlled(MOVE_QUEUE_SESSION_ID, &ProcessExecutor)
    })
    .unwrap();
    assert!(
        crate::database::load_session_record(MOVE_QUEUE_SESSION_ID)
            .unwrap()
            .is_none()
    );
    assert!(root.join("stopped").exists());
    assert!(
        controller
            .scan_orphan_workers(&ProcessExecutor, true)
            .candidates
            .is_empty()
    );
}

#[cfg(unix)]
// Hard-won: 6f0f854: unreadable genesis was mistaken for acknowledgement pruning.
#[test]
fn adoption_surfaces_corruption_without_installing_or_acknowledging_a_cut() {
    let name = test_name("adoption_surfaces_corruption_without_installing_or_acknowledging_a_cut");
    if !isolated_test_child(&name, "MJ_ADOPT_CORRUPT_GENESIS_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    let _proxy_guard =
        install_move_queue_relay_worker(&root, &directory.path().join("marker"), true);
    let mut relay = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "current").unwrap();
    for index in 0..80 {
        relay
            .record_observation(mj_core::relay::RelayObservation::Warning {
                message: format!("{index}:{}", "x".repeat(64 * 1024)),
            })
            .unwrap();
    }
    drop(relay);
    let mut sealed = fs::read_dir(root.join(mj_core::relay::RELAY_JOURNAL_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "gz"))
        .collect::<Vec<_>>();
    sealed.sort();
    assert!(sealed.len() >= 3);
    fs::write(&sealed[1], b"not a gzip stream").unwrap();
    crate::database::save_session(&checkpoint_test_session(MOVE_QUEUE_SESSION_ID)).unwrap();
    let spec = CommandSpec::new(root.join("hel").to_str().unwrap(), Vec::<String>::new());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let error = crate::session_manager::StandaloneSession::connect_adopting(
                &spec,
                MOVE_QUEUE_SESSION_ID,
            )
            .await
            .err()
            .expect("corrupt span cannot authorize a cut");
            assert!(format!("{error:#}").contains("unreadable"), "{error:#}");
        });
    assert!(!root.join("baseline-frontier").exists());
    let baseline = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "current")
        .unwrap()
        .live_session_baseline()
        .unwrap();
    assert_eq!(baseline.state.acknowledged_through, 0);
    assert!(
        crate::database::materialized_event_frontier(MOVE_QUEUE_SESSION_ID)
            .unwrap()
            .is_none_or(|(ordinal, _)| ordinal == 0)
    );
}

#[cfg(unix)]
#[test]
fn adoption_draining_cancels_the_live_cut_existence_probe_and_releases_ownership() {
    adoption_interrupted_probe(
        true,
        AdoptionProbe::Drain,
        "adoption_draining_cancels_the_live_cut_existence_probe_and_releases_ownership",
    );
}

#[cfg(unix)]
#[test]
fn adoption_draining_cancels_the_genesis_existence_probe_and_releases_ownership() {
    adoption_interrupted_probe(
        false,
        AdoptionProbe::Drain,
        "adoption_draining_cancels_the_genesis_existence_probe_and_releases_ownership",
    );
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum AdoptionProbe {
    Drain,
    Disconnect,
    Install,
    Unusable(&'static str),
}

#[cfg(unix)]
fn adoption_interrupted_probe(trimmed: bool, disposition: AdoptionProbe, short: &str) {
    let name = test_name(short);
    if !isolated_test_child(&name, "MJ_ADOPT_DRAINING_PROBE_CHILD") {
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(MOVE_QUEUE_SESSION_ID);
    let _proxy_guard =
        install_move_queue_relay_worker(&root, &directory.path().join("marker"), true);
    let mut relay = DurableRelay::open(&root, MOVE_QUEUE_SESSION_ID, "test").unwrap();
    if trimmed {
        use mj_core::native_agent::{NativeAgentCapabilities, NativeAgentEvent};
        relay
            .record_observation(mj_core::relay::RelayObservation::NativeAgent {
                event: NativeAgentEvent::ReplayBegin,
            })
            .unwrap();
        relay
            .record_observation(mj_core::relay::RelayObservation::NativeAgent {
                event: NativeAgentEvent::Spawned {
                    session_id: "staged-child".into(),
                    parent_session_id: None,
                    name: "child".into(),
                    task: "work".into(),
                    capabilities: NativeAgentCapabilities::default(),
                },
            })
            .unwrap();
        let response = relay.handle(mj_core::relay::RelayRequestEnvelope {
            request_id: "collect-before-adoption".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Acknowledge {
                through_ordinal: relay.latest_ordinal(),
                through_digest: relay.latest_digest().into(),
            },
        });
        assert!(matches!(
            response.body,
            mj_core::relay::RelayResponseBody::Ok { .. }
        ));
        fs::write(root.join("block-after-cut"), b"1").unwrap();
    }
    drop(relay);
    if matches!(
        disposition,
        AdoptionProbe::Drain | AdoptionProbe::Disconnect
    ) {
        fs::write(root.join("block-install-status"), b"1").unwrap();
    }
    if let AdoptionProbe::Unusable(kind) = disposition {
        fs::write(root.join("unusable-worker"), kind).unwrap();
    }
    let mut record = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    record.state = SessionState::Disconnected;
    record.native_session_id = None;
    record.target_template_id = "local-bare".into();
    record.target = Some(Box::new(TargetLocator::LocalBare {
        worker_root: root.clone(),
    }));
    crate::database::save_session(&record).unwrap();
    let mut config = Config::default();
    config.targets.insert(
        "local-bare".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    let mut controller = Controller {
        config,
        state: crate::database::load_state().unwrap(),
    };
    let gate = std::sync::Arc::new(crate::recovery_gate::RecoveryGate::default());
    let owner_gate = gate.clone();
    let request = mj_client::daemon::RecoveryAdoptionRequest {
        session_id: MOVE_QUEUE_SESSION_ID.into(),
        target_id: "local-bare".into(),
        profile: None,
        bundle: None,
        all_instances: false,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let stopped = cancellation.clone();
    runtime.block_on(async {
        let work = tokio::spawn(async move {
            controller
                .adopt_orphan_worker_controlled(
                    &request,
                    &ProcessExecutor,
                    crate::controller::AdoptionControl {
                        cancelled: stopped,
                        progress: None,
                        lifecycle_gate: owner_gate,
                    },
                )
                .await
        });
        if let AdoptionProbe::Unusable(_) = disposition {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), work)
                .await
                .unwrap()
                .unwrap();
            assert!(format!("{:#}", outcome.result.unwrap_err()).contains("no runnable harness"));
            let current = crate::database::read_durable_session_record(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .unwrap();
            assert_eq!(current.state, SessionState::Disconnected);
            assert_eq!(current.target, record.target);
            assert!(
                crate::database::materialized_event_frontier(MOVE_QUEUE_SESSION_ID)
                    .unwrap()
                    .is_none_or(|(ordinal, _)| ordinal == 0)
            );
            return;
        }
        if matches!(disposition, AdoptionProbe::Install) {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), work)
                .await
                .unwrap()
                .unwrap();
            outcome.result.unwrap();
            let admission = outcome.admission.unwrap();
            let refused = gate
                .try_start(
                    MOVE_QUEUE_SESSION_ID,
                    crate::recovery_gate::BackgroundWork::WorkerReplacement,
                )
                .err()
                .expect("install must retain session ownership until reply");
            assert!(refused.holders.contains(&"adoption install"));
            let current = crate::database::read_durable_session_record(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .unwrap();
            assert_eq!(current.state, SessionState::Running);
            assert!(
                current.native_session_id.is_none(),
                "adoption must not wait for native readiness"
            );
            assert!(
                crate::database::materialized_event_frontier(MOVE_QUEUE_SESSION_ID)
                    .unwrap()
                    .unwrap()
                    .0
                    > 0
            );
            drop(admission);
            drop(
                gate.try_start(
                    MOVE_QUEUE_SESSION_ID,
                    crate::recovery_gate::BackgroundWork::WorkerReplacement,
                )
                .expect("reply releases session ownership"),
            );
            return;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !root.join("install-status-reached").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !crate::upgrade::active_labels()
                .iter()
                .any(|label| label.contains("adoption"))
        );
        if matches!(disposition, AdoptionProbe::Disconnect) {
            cancellation.cancel();
        } else {
            crate::upgrade::gate().begin_draining();
        }
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), work)
            .await
            .unwrap()
            .unwrap();
        assert!(format!("{:#}", outcome.result.unwrap_err()).contains(
            if matches!(disposition, AdoptionProbe::Disconnect) {
                "cancelled"
            } else {
                "handoff"
            }
        ));
        assert!(outcome.admission.is_none());
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let destroy_owner = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            gate.enter_foreground(
                MOVE_QUEUE_SESSION_ID,
                "destroy recovered environment",
                crate::recovery_gate::LifecycleOrigin::Person,
                &cancelled,
                &ProcessExecutor,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            crate::database::read_durable_session_record(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .unwrap(),
            record
        );
        assert!(
            crate::database::materialized_event_frontier(MOVE_QUEUE_SESSION_ID)
                .unwrap()
                .is_none_or(|(ordinal, _)| ordinal == 0)
        );
        drop(destroy_owner);
        fs::write(root.join("release-install-status"), b"1").unwrap();
    });
}

#[cfg(unix)]
#[test]
fn adoption_disconnecting_cancels_the_existence_probe_and_releases_ownership() {
    adoption_interrupted_probe(
        true,
        AdoptionProbe::Disconnect,
        "adoption_disconnecting_cancels_the_existence_probe_and_releases_ownership",
    );
}

#[cfg(unix)]
#[test]
fn adoption_install_keeps_both_owners_through_publication_until_the_reply() {
    adoption_interrupted_probe(
        true,
        AdoptionProbe::Install,
        "adoption_install_keeps_both_owners_through_publication_until_the_reply",
    );
}

#[cfg(unix)]
// Hard-won: f30ad64: a failed adoption assertion left a proxy, relay child and grep running.
#[test]
fn proxy_fixture_reaps_its_pipeline_when_an_assertion_unwinds() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("blocked-worker");
    let group = std::cell::Cell::new(None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = install_move_queue_relay_worker(&root, &root.join("served"), true);
        fs::write(root.join("block-install-status"), b"1").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let spec = CommandSpec::new(
                root.join("hel").to_string_lossy(),
                std::iter::empty::<String>(),
            );
            let mut client =
                crate::worker_client::RelayClient::connect(&spec, MOVE_QUEUE_SESSION_ID)
                    .await
                    .unwrap();
            group.set(Some(guard.registered_group()));
            let status = client.status();
            tokio::pin!(status);
            tokio::select! {
                response = &mut status => panic!("status did not block: {response:?}"),
                () = async {
                    while !root.join("install-status-reached").exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                } => {}
            }
            assert!(
                group.get().is_none(),
                "forced assertion while the proxy pipeline is blocked"
            );
        });
    }));
    assert!(result.is_err());
    assert!(
        !TestProxyGroupGuard::group_exists(group.get().unwrap()).unwrap(),
        "guard returned before the pipeline was reaped"
    );
    // Failed spawn/handshake may never write a launch marker.
    drop(TestProxyGroupGuard::new(&root.join("never-spawned")));
}

#[cfg(unix)]
// Hard-won: d5c0c25: a Status reply from a checkpoint-only or closed worker was published as Running.
#[test]
fn adoption_rejects_checkpoint_only_and_closed_workers() {
    let name = test_name("adoption_rejects_checkpoint_only_and_closed_workers");
    if !isolated_test_child(&name, "MJ_ADOPT_DRAINING_PROBE_CHILD") {
        return;
    }
    for kind in ["checkpoint-only", "closed", "closing"] {
        adoption_interrupted_probe(
            true,
            AdoptionProbe::Unusable(kind),
            "adoption_rejects_checkpoint_only_and_closed_workers",
        );
    }
}

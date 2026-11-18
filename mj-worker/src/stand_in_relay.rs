//! Stand-in relays for the controller and daemon tests.
//!
//! A test that needs a worker relay without a worker starts its test binary
//! again as the relay: the child runs one `--exact` test that calls a function
//! here, which serves the relay protocol on standard input and output from a
//! real relay journal. The test binaries of mj-controller and mj-daemon both
//! define those tests, under the names in `mj_core::test_hooks`, so either
//! can start the relay. The environment each relay reads is defined there.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
use mj_core::config::HarnessKind;
use mj_core::relay::{RelayCommand, RelayCommandOutcome};

use mj_core::test_hooks::latch_relay::*;
use mj_core::test_hooks::leased_relay::*;

/// Relay server half of the checkpoint latch test.
///
/// A durable relay only reports a checkpoint barrier ready once a dispatch
/// driver claims it, so this also runs the one step the worker runtime
/// performs for a barrier. It does nothing unless a parent test points it
/// at a relay journal root.
pub fn serve_latch_relay() {
    let Some(root) = std::env::var_os(LATCH_RELAY_ROOT) else {
        return;
    };
    // With `--nocapture` libtest writes `test <name> ... ` without a
    // trailing newline before the body runs. End that line first so it
    // cannot glue itself onto the first protocol frame.
    println!();
    let mut resume_discarded = false;
    let checkpoint_only = std::env::var_os(LATCH_CHECKPOINT_ONLY).is_some();
    // Queue-only connections never own or write the control journal.
    let mut relay: Option<crate::relay::DurableRelay> = None;
    let ready_at = Instant::now()
        + Duration::from_millis(
            std::env::var(LATCH_RELAY_STARTUP_DELAY_MS)
                .ok()
                .map(|value| value.parse::<u64>().unwrap())
                .unwrap_or(0),
        );
    let reject_release = std::env::var_os(LATCH_RELAY_REJECT_RELEASE).is_some();
    #[cfg(unix)]
    let late_write = std::env::var(LATCH_RELAY_JOURNALS_BEFORE_CLOSE).ok();
    #[cfg(unix)]
    let journals_before_close = late_write.is_some();
    #[cfg(unix)]
    let running = std::env::var_os(LATCH_RELAY_RUNNING).is_some();
    #[cfg(unix)]
    let answer_prompts = std::env::var_os(LATCH_RELAY_ANSWER_PROMPTS).is_some();
    let mut reader = std::io::stdin().lock();
    let mut writer = std::io::stdout().lock();
    let mut configured = false;
    while let Some(request) =
        mj_core::relay::read_relay_frame(&mut reader).expect("read a relay request")
    {
        if std::env::var_os(LATCH_REJECT_PROJECT_MEMORY).is_some() {
            assert!(
                !request.request.method_name().contains("project_memory"),
                "idle worker upgrade must not require a project-memory transfer"
            );
        }
        let max_protocol = std::env::var(LATCH_RELAY_MAX_PROTOCOL)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .or(Some(mj_core::relay::RELAY_CHANGE_WAIT_PROTOCOL));
        if Path::new(&root).join("upgrade-goal-peer").exists() {
            use std::io::Write as _;
            let name = match &request.request {
                mj_core::relay::RelayRequest::Submit {
                    command: RelayCommand::BeginCheckpoint { .. },
                    ..
                } => "begin_checkpoint",
                mj_core::relay::RelayRequest::Submit {
                    command: RelayCommand::CancelTurn,
                    ..
                } => "cancel_turn",
                _ => request.request.method_name(),
            };
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(Path::new(&root).join("upgrade-requests.log"))
                .unwrap();
            writeln!(log, "{name}").unwrap();
            if max_protocol
                .is_some_and(|version| version < mj_core::relay::RELAY_IDLE_GOAL_INTENT_PROTOCOL)
                && let mj_core::relay::RelayRequest::ReserveIdle { command_id } = &request.request
            {
                assert!(
                    relay.as_ref().is_some_and(|relay| relay
                        .operational_state()
                        .checkpoint_barrier
                        .as_deref()
                        == Some(command_id.as_str())),
                    "old worker's initial ReserveIdle rejects goal intent; only accepted-barrier confirmation is valid"
                );
            }
        }
        let payload = match &request.request {
            mj_core::relay::RelayRequest::Hello { supported, .. } => {
                let negotiated = max_protocol
                    .unwrap_or(mj_core::relay::RELAY_PROTOCOL_VERSION)
                    .min(supported.max);
                Some(mj_core::relay::RelayResponsePayload::Hello {
                    negotiated,
                    relay_version: if max_protocol.is_some() {
                        "older-protocol-test"
                    } else {
                        "1.0.0"
                    }
                    .into(),
                    session_id: LATCH_RELAY_SESSION.into(),
                    worker_build: None,
                })
            }
            mj_core::relay::RelayRequest::PollSubagentQueue { .. }
            | mj_core::relay::RelayRequest::WaitForChange { .. }
            | mj_core::relay::RelayRequest::SubagentRequests => {
                subagent_connection_response(&request).map(|response| match response.body {
                    mj_core::relay::RelayResponseBody::Ok { payload } => payload,
                    _ => unreachable!(),
                })
            }
            _ => None,
        };
        if let Some(payload) = payload {
            let protocol_version = match &payload {
                mj_core::relay::RelayResponsePayload::Hello { negotiated, .. } => *negotiated,
                _ => request.protocol_version,
            };
            mj_core::relay::write_relay_frame(
                &mut writer,
                &mj_core::relay::RelayResponseEnvelope {
                    request_id: request.request_id,
                    protocol_version,
                    body: mj_core::relay::RelayResponseBody::Ok { payload },
                },
            )
            .expect("answer a queue or hello request");
            continue;
        }
        // A checkpoint carries project memory on a second connection while
        // the first still owns the control journal. A worker serves memory at
        // the connection layer, without the journal; this stand-in has no
        // memory endpoint, so it answers as `DurableRelay` does instead of
        // opening a second journal writer.
        if matches!(
            &request.request,
            mj_core::relay::RelayRequest::ProjectMemorySnapshot
                | mj_core::relay::RelayRequest::ProjectMemoryTransfer { .. }
                | mj_core::relay::RelayRequest::InstallProjectMemorySnapshot { .. }
                | mj_core::relay::RelayRequest::ReplaceProjectMemoryTree { .. }
        ) {
            mj_core::relay::write_relay_frame(
                &mut writer,
                &mj_core::relay::RelayResponseEnvelope {
                    request_id: request.request_id,
                    protocol_version: request.protocol_version,
                    body: mj_core::relay::relay_error(
                        mj_core::relay::RelayErrorCode::InvalidState,
                        "connection-only requests must be handled by the live relay transport",
                        false,
                        None,
                    ),
                },
            )
            .expect("answer a project-memory request");
            continue;
        }
        let relay = relay.get_or_insert_with(|| {
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
            let mut relay = if checkpoint_only {
                crate::relay::DurableRelay::open_for_checkpoint(
                    Path::new(&root),
                    LATCH_RELAY_SESSION,
                    "1.0.0",
                )
            } else {
                crate::relay::DurableRelay::open(Path::new(&root), LATCH_RELAY_SESSION, "1.0.0")
            }
            .expect("open the test relay journal");
            relay.set_turn_verdict_harness(HarnessKind::Codex);
            if relay.operational_state().native_session_id.is_none() {
                if let Ok(fresh) = std::env::var(LATCH_RELAY_FRESH_NATIVE) {
                    let replaced = std::env::var(LATCH_RELAY_REPLACED_UNUSED).ok();
                    // What a worker restored from a checkpoint journals when the
                    // harness cannot reload the archived native session and this
                    // session never used it.
                    relay
                        .record_observation(mj_core::relay::RelayObservation::SessionRestarted)
                        .unwrap();
                    relay
                        .record_observation(mj_core::relay::RelayObservation::Warning {
                            message: "Claude Code has no native session native-session and this \
                                      session never used it; continuing in a new empty session"
                                .into(),
                        })
                        .unwrap();
                    relay
                        .record_observation(mj_core::relay::RelayObservation::SessionOpened {
                            native_session_id: fresh,
                            native_continuity_lost: false,
                            resumed: false,
                            replaced_unused_native_session_id: replaced,
                        })
                        .unwrap();
                } else {
                    relay
                        .record_observation(mj_core::relay::RelayObservation::SessionOpened {
                            native_session_id: "native-session".into(),
                            native_continuity_lost: false,
                            resumed: true,
                            replaced_unused_native_session_id: None,
                        })
                        .unwrap();
                }
            }
            if !relay.operational_state().goal.synchronized() && !checkpoint_only {
                relay.record_session_update(serde_json::from_value(serde_json::json!({
                "sessionUpdate":"session_info_update", "_meta":{"goal":null,"execution":{"version":1,"status":"idle"}}
            })).unwrap()).unwrap();
            }
            #[cfg(unix)]
            if running && relay.operational_state().active_prompt.is_none()
                && !Path::new(&root).join("cancellation-accepted").exists()
            {
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
            relay
        });
        if matches!(&request.request, mj_core::relay::RelayRequest::Submit { command_id, command: RelayCommand::ClearQueuedPrompts } if command_id == "discard-before-ready")
        {
            resume_discarded = true;
        }
        let discard_ready = !Path::new(&root)
            .join("resume-discard-before-ready")
            .exists()
            || resume_discarded;
        if !checkpoint_only && !configured && Instant::now() >= ready_at && discard_ready {
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
        if std::env::var("MJ_TEST_QUOTA_BARRIER_RACE").as_deref() == Ok("1") {
            if matches!(
                &request.request,
                mj_core::relay::RelayRequest::Submit {
                    command: RelayCommand::BeginCheckpoint { .. },
                    ..
                }
            ) {
                relay
                    .record_observation(mj_core::relay::RelayObservation::HarnessTurnStarted {
                        started_at_ms: mj_core::clock::epoch_millis(),
                    })
                    .unwrap();
            }
            assert!(
                !matches!(
                    &request.request,
                    mj_core::relay::RelayRequest::Submit {
                        command: RelayCommand::CancelTurn,
                        ..
                    }
                ),
                "automatic quota Move cancelled new work"
            );
        }
        let late_turn = Path::new(&root).join("upgrade-late-turn");
        if late_turn.exists()
            && matches!(
                request.request,
                mj_core::relay::RelayRequest::Status | mj_core::relay::RelayRequest::Attach { .. }
            )
        {
            std::fs::remove_file(late_turn).unwrap();
            relay
                .record_observation(mj_core::relay::RelayObservation::HarnessTurnStarted {
                    started_at_ms: mj_core::clock::epoch_millis(),
                })
                .unwrap();
            relay
                .record_observation(mj_core::relay::RelayObservation::HarnessTurnSettled {
                    autonomous: None,
                    origin: Some("test autonomous completion".into()),
                    prompt_in_flight: false,
                })
                .unwrap();
        }
        #[cfg(unix)]
        hold_named_relay_command(&request.request);
        // The harness keeps talking while a checkpoint barrier holds: this
        // lands one write of its own after the daemon's last look and before
        // its Close, through the same relay calls the worker runtime uses.
        #[cfg(unix)]
        if matches!(
            &request.request,
            mj_core::relay::RelayRequest::Submit {
                command: RelayCommand::Close { .. },
                ..
            }
        ) {
            match late_write.as_deref() {
                Some(LATE_HARNESS_NOTIFICATION) => {
                    relay
                        .record_session_update(
                            serde_json::from_value(serde_json::json!({
                                "sessionUpdate": "session_info_update",
                                "title": "A title the harness chose after the turn"
                            }))
                            .unwrap(),
                        )
                        .expect("record a harness notification");
                }
                Some(LATE_HARNESS_QUESTION) => {
                    relay
                        .record_observation(
                            mj_core::relay::RelayObservation::ElicitationRequested {
                                request: mj_core::elicitation::ElicitationRequest {
                                    id: "late-question".into(),
                                    message: "May I continue?".into(),
                                    title: None,
                                    description: None,
                                    fields: Vec::new(),
                                    tool_permission: None,
                                },
                            },
                        )
                        .expect("record a harness question");
                }
                Some(other) => panic!("unknown late write {other:?}"),
                None => {}
            }
        }
        if std::env::var_os(LATCH_RECORD_SUBAGENT_ADMISSION).is_some()
            && let mj_core::relay::RelayRequest::SetSubagentAdmission { open } = &request.request
        {
            record_test_subagent_admission(Path::new(&root), *open);
            let marker = Path::new(&root).join("drop-admission-close-reply-once");
            if !open
                && std::env::var_os(LATCH_DROP_ADMISSION_CLOSE_REPLY).is_some()
                && !marker.exists()
            {
                std::fs::write(marker, b"dropped").expect("record the dropped close reply");
                break;
            }
        }
        let response = if Path::new(&root).join("upgrade-held-reviewer").exists()
            && matches!(
                request.request,
                mj_core::relay::RelayRequest::ReserveIdle { .. }
            ) {
            mj_core::relay::RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: request.protocol_version,
                body: mj_core::relay::RelayResponseBody::Ok {
                    payload: mj_core::relay::RelayResponsePayload::IdleReservation {
                        ordinal: None,
                    },
                },
            }
        } else if reject_release && requests_checkpoint_release(&request) {
            unparseable_request_response(&request)
        } else if matches!(
            request.request,
            mj_core::relay::RelayRequest::SetOperatorAway { .. }
        ) {
            mj_core::relay::RelayResponseEnvelope {
                request_id: request.request_id.clone(),
                protocol_version: request.protocol_version,
                body: mj_core::relay::RelayResponseBody::Ok {
                    payload: mj_core::relay::RelayResponsePayload::Status(
                        relay.operational_state(),
                    ),
                },
            }
        } else if let Some(response) = subagent_connection_response(&request) {
            response
        } else {
            let mut request = request;
            if max_protocol.is_some() {
                // Run the current durable relay's behavior while speaking
                // the older version negotiated by this test worker.
                request.protocol_version = mj_core::relay::RELAY_PROTOCOL_VERSION;
            }
            let mut response = relay.handle(request);
            if let Some(max_protocol) = max_protocol {
                response.protocol_version =
                    max_protocol.min(mj_core::relay::RELAY_PROTOCOL_VERSION);
            }
            response
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
                RelayCommand::Prompt { .. } if answer_prompts => {
                    relay
                        .record_session_update(
                            serde_json::from_value(serde_json::json!({
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": LATCH_RELAY_ANSWER}
                            }))
                            .unwrap(),
                        )
                        .expect("answer the prompt");
                    relay
                        .record_command_completed(
                            &claimed.command_id,
                            RelayCommandOutcome::Prompt {
                                work_facts: None,
                                diagnostic: None,
                                stop_reason: "end_turn".into(),
                                usage: None,
                            },
                        )
                        .expect("complete the answered prompt");
                }
                #[cfg(unix)]
                RelayCommand::Close { .. } if journals_before_close => {
                    relay
                        .record_command_completed(&claimed.command_id, RelayCommandOutcome::Closed)
                        .expect("close the relay");
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
                    if Path::new(&root).join("unsettled-cancellation").exists() {
                        std::fs::write(Path::new(&root).join("cancellation-accepted"), b"accepted")
                            .unwrap();
                        continue;
                    }
                    relay
                        .record_command_completed(
                            &prompt_id,
                            RelayCommandOutcome::Prompt {
                                work_facts: None,
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

/// See [`LATCH_RELAY_HOLD_COMMAND`]. Only the first matching request waits.
#[cfg(unix)]
fn hold_named_relay_command(request: &mj_core::relay::RelayRequest) {
    let (Ok(command), Some(path)) = (
        std::env::var(LATCH_RELAY_HOLD_COMMAND),
        std::env::var_os(LATCH_RELAY_HOLD_PATH),
    ) else {
        return;
    };
    let matches = match request {
        mj_core::relay::RelayRequest::Submit {
            command: RelayCommand::BeginCheckpoint { .. },
            ..
        } => command == "begin_checkpoint",
        mj_core::relay::RelayRequest::Submit {
            command: RelayCommand::Close { .. },
            ..
        } => command == "close",
        _ => false,
    };
    let path = PathBuf::from(path);
    let reached = path.with_extension("reached");
    if !matches || reached.exists() {
        return;
    }
    std::fs::write(&reached, b"held").expect("record the held relay command");
    let release = path.with_extension("release");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() {
        assert!(
            Instant::now() < deadline,
            "the parent test never released the held {command}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn test_subagent_queue_path(relay_root: &Path) -> PathBuf {
    relay_root
        .parent()
        .expect("relay root parent")
        .join(LATCH_RELAY_SESSION)
        .join("subagents.json")
}
fn read_test_subagent_queue(path: &Path) -> serde_json::Value {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("parse fixture sub-agent queue"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::json!({"requests": {}, "results": {}})
        }
        Err(error) => panic!("read fixture sub-agent queue: {error}"),
    }
}
fn record_test_subagent_admission(relay_root: &Path, open: bool) {
    let queue_path = test_subagent_queue_path(relay_root);
    let mut queue = read_test_subagent_queue(&queue_path);
    let transitions = queue["admission_transitions"].as_array_mut().map(|items| {
        items.push(serde_json::Value::Bool(open));
    });
    if transitions.is_none() {
        queue["admission_transitions"] = serde_json::json!([open]);
    }
    queue["mutating_admission_open"] = serde_json::Value::Bool(open);
    mj_core::config::atomic_write(&queue_path, &serde_json::to_vec(&queue).unwrap())
        .expect("persist fake worker sub-agent admission");
}
fn subagent_connection_response(
    request: &mj_core::relay::RelayRequestEnvelope,
) -> Option<mj_core::relay::RelayResponseEnvelope> {
    let queue_path =
        std::env::var_os(LATCH_RELAY_ROOT).map(|root| test_subagent_queue_path(Path::new(&root)));
    let mut queue = queue_path
        .as_ref()
        .map(|path| read_test_subagent_queue(path))
        .unwrap_or_else(|| serde_json::json!({"requests": {}, "results": {}}));
    let requests = || {
        queue["requests"]
            .as_object()
            .into_iter()
            .flat_map(|items| items.values())
            .map(|value| {
                serde_json::from_value::<mj_core::subagent::SubagentToolRequest>(value.clone())
                    .unwrap()
            })
            .collect::<Vec<_>>()
    };
    let results = || {
        queue["results"]
            .as_object()
            .into_iter()
            .flat_map(|items| items.values())
            .map(|value| {
                serde_json::from_value::<mj_core::subagent::SubagentToolResult>(value.clone())
                    .unwrap()
            })
            .collect::<Vec<_>>()
    };
    let payload = match &request.request {
        mj_core::relay::RelayRequest::PollSubagentQueue { acknowledged } => {
            mj_core::relay::RelayResponsePayload::SubagentQueue {
                page: mj_core::subagent::SubagentQueuePage {
                    sequence: acknowledged.unwrap_or(0) + 1,
                    changes: requests()
                        .into_iter()
                        .map(mj_core::subagent::SubagentQueueChange::Request)
                        .chain(results().iter().map(|result| {
                            mj_core::subagent::SubagentQueueChange::Completed(
                                mj_core::subagent::SubagentCompletion::from(result),
                            )
                        }))
                        .collect(),
                    has_more: false,
                },
            }
        }
        mj_core::relay::RelayRequest::SubagentRequests => {
            mj_core::relay::RelayResponsePayload::SubagentRequests {
                requests: requests(),
                results: results(),
            }
        }
        mj_core::relay::RelayRequest::CompleteSubagentRequest { result } => {
            let path = queue_path.as_ref().expect("fixture queue root");
            queue["requests"]
                .as_object_mut()
                .unwrap()
                .remove(&result.request_id);
            queue["results"].as_object_mut().unwrap().insert(
                result.request_id.clone(),
                serde_json::to_value(result).unwrap(),
            );
            mj_core::config::atomic_write(path, &serde_json::to_vec(&queue).unwrap()).unwrap();
            use std::io::Write as _;
            writeln!(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.with_extension("deliveries"))
                    .unwrap(),
                "{}",
                result.request_id
            )
            .unwrap();
            mj_core::relay::RelayResponsePayload::SubagentRequestCompleted
        }
        mj_core::relay::RelayRequest::SetSubagentAdmission { open } => {
            mj_core::relay::RelayResponsePayload::SubagentAdmissionChanged { open: *open }
        }
        // This fake's queue never changes and its relay is served elsewhere:
        // hold briefly, as the worker would, and report nothing new.
        mj_core::relay::RelayRequest::WaitForChange {
            after_ordinal,
            state_digest,
        } => {
            std::thread::sleep(std::time::Duration::from_millis(200));
            mj_core::relay::RelayResponsePayload::ChangeObserved {
                latest_ordinal: *after_ordinal,
                state_digest: state_digest.clone().unwrap_or_default(),
                subagent_queue_changed: !requests().is_empty() || !results().is_empty(),
            }
        }
        _ => return None,
    };
    Some(mj_core::relay::RelayResponseEnvelope {
        request_id: request.request_id.clone(),
        protocol_version: request.protocol_version,
        body: mj_core::relay::RelayResponseBody::Ok { payload },
    })
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

/// Relay server half of the leased-submission tests. It does nothing unless
/// a parent test points it at a relay journal root.
pub fn serve_leased_relay() {
    let Some(root) = std::env::var_os(LEASED_RELAY_ROOT) else {
        return;
    };
    let memory_protocol = {
        let path = PathBuf::from(&root).join("memory-sync-protocol");
        path.exists().then(|| {
            std::fs::read_to_string(path)
                .unwrap()
                .parse::<u32>()
                .unwrap()
        })
    };
    // With `--nocapture` libtest writes `test <name> ... ` without a
    // trailing newline before the body runs. End that line first so it
    // cannot glue itself onto the first protocol frame.
    println!();
    let mut relay = None;
    {
        let marker = std::env::var_os("MJ_TEST_BLOCKED_REVIEWER");
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        let mut subagent_known_ids = std::collections::BTreeSet::new();
        // The queue files this connection last reported to a change wait.
        let mut reported_queue: Option<[Option<Vec<u8>>; 2]> = None;
        while let Some(request) = mj_core::relay::read_relay_frame(&mut input).unwrap() {
            use mj_core::relay::{
                RelayRequest, RelayResponseBody, RelayResponseEnvelope, RelayResponsePayload,
            };
            if matches!(request.request, RelayRequest::PollSubagentQueue { .. })
                && PathBuf::from(&root)
                    .join("disconnect-subagent-queue")
                    .exists()
            {
                return;
            }
            // One line per request, so a parent test can count round trips.
            {
                use std::io::Write as _;
                let mut log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(PathBuf::from(&root).join(RELAY_REQUEST_LOG))
                    .unwrap();
                writeln!(log, "{}", request.request.method_name()).unwrap();
            }
            if PathBuf::from(&root).join("worker-unreachable").exists() {
                return;
            }
            if request.request.method_name() == "project_memory_transfer"
                && PathBuf::from(&root).join("drop-project-memory").exists()
            {
                return;
            }
            if matches!(
                &request.request,
                RelayRequest::Submit {
                    command: RelayCommand::SetQuotaRecovery { .. },
                    ..
                }
            ) {
                let blocked = PathBuf::from(&root).join("block-quota-submit");
                if blocked.exists() {
                    std::fs::write(
                        PathBuf::from(&root).join("quota-submit-waiting"),
                        b"waiting",
                    )
                    .unwrap();
                    while blocked.exists() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
            let preparation: Option<mj_core::relay::HarnessPreparation> =
                std::fs::read(PathBuf::from(&root).join("preparation.json"))
                    .ok()
                    .map(|body| serde_json::from_slice(&body).unwrap());
            if preparation.is_some()
                && matches!(request.request, RelayRequest::SetSubagentAdmission { .. })
            {
                mj_core::relay::write_relay_frame(
                    &mut output,
                    &RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        body: RelayResponseBody::Error {
                            error: mj_core::relay::relay_protocol_error(
                                mj_core::relay::RelayErrorCode::InvalidState,
                                "harness preparation failed at harness-profile: missing staged Claude delegation server",
                                false,
                                None,
                            ),
                        },
                    },
                )
                .unwrap();
                continue;
            }
            if let RelayRequest::WaitForChange {
                after_ordinal,
                state_digest,
            } = &request.request
            {
                let blocked = PathBuf::from(&root).join("block-change-wait");
                while blocked.exists() {
                    if PathBuf::from(&root).join("worker-unreachable").exists() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                // Each connection to this fake is its own process, and only the
                // session's own connection may open the relay journal. So this
                // wait reports the relay as the controller named it and holds,
                // like the real worker, until the queue files it rereads change.
                let queue_files = || {
                    ["subagent-request.json", "subagent-result.json"]
                        .map(|name| std::fs::read(PathBuf::from(&root).join(name)).ok())
                };
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                let queue_changed = loop {
                    if PathBuf::from(&root).join("worker-unreachable").exists() {
                        return;
                    }
                    let queue = queue_files();
                    if reported_queue.as_ref() != Some(&queue) {
                        reported_queue = Some(queue);
                        break true;
                    }
                    if std::time::Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                mj_core::relay::write_relay_frame(
                    &mut output,
                    &RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        body: RelayResponseBody::Ok {
                            payload: RelayResponsePayload::ChangeObserved {
                                latest_ordinal: *after_ordinal,
                                state_digest: state_digest.clone().unwrap_or_default(),
                                subagent_queue_changed: queue_changed,
                            },
                        },
                    },
                )
                .unwrap();
                continue;
            }
            let history = match &request.request {
                RelayRequest::Hello { .. } => Some(RelayResponsePayload::Hello {
                    negotiated: memory_protocol.unwrap_or(mj_core::relay::RELAY_PROTOCOL_VERSION),
                    relay_version: "1.0.0".into(),
                    session_id: LEASED_RELAY_SESSION.into(),
                    worker_build: None,
                }),
                RelayRequest::CpuUsage => {
                    let path = PathBuf::from(&root).join("cpu-usage.json");
                    let usage = if path.exists() {
                        Some(serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
                    } else {
                        None
                    };
                    Some(RelayResponsePayload::CpuUsage { usage })
                }
                RelayRequest::SubagentRequests | RelayRequest::PollSubagentQueue { .. } => {
                    let path = PathBuf::from(&root).join("subagent-request.json");
                    let requests = if path.exists() {
                        vec![serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()]
                    } else {
                        Vec::new()
                    };
                    let path = PathBuf::from(&root).join("subagent-result.json");
                    let results = if path.exists() {
                        vec![serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()]
                    } else {
                        Vec::new()
                    };
                    if let RelayRequest::PollSubagentQueue { acknowledged } = &request.request {
                        use mj_core::subagent::{
                            SubagentCompletion, SubagentQueueChange, SubagentQueuePage,
                        };
                        let mut changes = requests
                            .into_iter()
                            .map(SubagentQueueChange::Request)
                            .chain(results.iter().map(|r| {
                                SubagentQueueChange::Completed(SubagentCompletion::from(r))
                            }))
                            .collect::<Vec<_>>();
                        let ids = changes
                            .iter()
                            .map(|c| c.request_id().to_owned())
                            .collect::<std::collections::BTreeSet<_>>();
                        changes.extend(subagent_known_ids.difference(&ids).map(|id| {
                            SubagentQueueChange::Retired {
                                request_id: id.clone(),
                            }
                        }));
                        subagent_known_ids = ids;
                        Some(RelayResponsePayload::SubagentQueue {
                            page: SubagentQueuePage {
                                sequence: acknowledged.unwrap_or(0) + 1,
                                changes,
                                has_more: false,
                            },
                        })
                    } else {
                        Some(RelayResponsePayload::SubagentRequests { requests, results })
                    }
                }
                RelayRequest::CompleteSubagentRequest { result } => {
                    if PathBuf::from(&root)
                        .join("slow-delegation-delivery")
                        .exists()
                    {
                        std::thread::sleep(Duration::from_secs(6));
                    }
                    std::fs::write(
                        PathBuf::from(&root).join("subagent-result.json"),
                        serde_json::to_vec(result).unwrap(),
                    )
                    .unwrap();
                    let path = PathBuf::from(&root).join("subagent-request.json");
                    if path.exists() {
                        std::fs::remove_file(path).unwrap();
                    }
                    Some(RelayResponsePayload::SubagentRequestCompleted)
                }
                RelayRequest::HistoryRequests => {
                    let path = PathBuf::from(&root).join("history-request.json");
                    let requests = if path.exists() {
                        vec![
                            serde_json::from_slice::<mj_core::history::HistoryRequest>(
                                &std::fs::read(path).unwrap(),
                            )
                            .unwrap(),
                        ]
                    } else {
                        Vec::new()
                    };
                    Some(RelayResponsePayload::HistoryRequests { requests })
                }
                RelayRequest::SetSubagentAdmission { open } => {
                    let admission = PathBuf::from(&root).join("subagent-admission.json");
                    let mut state: serde_json::Value = std::fs::read(&admission)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                        .unwrap_or_else(|| serde_json::json!({"open": true, "calls": 0}));
                    state["open"] = serde_json::Value::Bool(*open);
                    state["calls"] =
                        serde_json::Value::from(state["calls"].as_u64().unwrap_or_default() + 1);
                    std::fs::write(&admission, serde_json::to_vec(&state).unwrap()).unwrap();
                    let marker = PathBuf::from(&root).join("drop-admission-open-reply-once");
                    if *open
                        && std::env::var_os(DROP_ADMISSION_OPEN_REPLY).is_some()
                        && !marker.exists()
                    {
                        std::fs::write(marker, b"dropped").unwrap();
                        return;
                    }
                    Some(RelayResponsePayload::SubagentAdmissionChanged { open: *open })
                }
                RelayRequest::CompleteHistoryRequest { result } => {
                    std::fs::write(
                        PathBuf::from(&root).join("history-result.json"),
                        serde_json::to_vec(result).unwrap(),
                    )
                    .unwrap();
                    std::fs::remove_file(PathBuf::from(&root).join("history-request.json"))
                        .unwrap();
                    Some(RelayResponsePayload::HistoryRequestCompleted)
                }
                _ => None,
            };
            if let Some(payload) = history {
                let protocol_version = match &payload {
                    RelayResponsePayload::Hello { negotiated, .. } => *negotiated,
                    _ => request.protocol_version,
                };
                mj_core::relay::write_relay_frame(
                    &mut output,
                    &RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version,
                        body: RelayResponseBody::Ok { payload },
                    },
                )
                .unwrap();
                continue;
            }
            let relay = relay.get_or_insert_with(|| {
                let mut relay = crate::relay::DurableRelay::open(
                    std::path::Path::new(&root),
                    LEASED_RELAY_SESSION,
                    "1.0.0",
                )
                .expect("open the test relay journal");
                if std::env::var_os(CLEAR_MAILBOX_CONTROL).is_some() {
                    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Codex);
                    if relay.operational_state().native_session_id.is_none() {
                        relay
                            .record_observation(mj_core::relay::RelayObservation::SessionOpened {
                                native_session_id: "original".into(),
                                resumed: false,
                                native_continuity_lost: false,
                                replaced_unused_native_session_id: None,
                            })
                            .unwrap();
                    }
                }
                if PathBuf::from(&root)
                    .join("disconnect-subagent-queue")
                    .exists()
                {
                    relay.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
                }
                if std::env::var_os(RELAY_SESSION_CONFIGURED).is_some() {
                    // What a worker records once its harness session is open, so the
                    // relay reports no work in flight.
                    relay
                        .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
                            config_options: if std::env::var_os("MJ_TEST_MODEL_POLICY").is_some() {
                                policy_test_options()
                            } else {
                                vec![]
                            },
                        })
                        .expect("record the configured harness session");
                    if std::env::var_os("MJ_TEST_MODEL_POLICY").is_some() {
                        relay
                            .record_observation(mj_core::relay::RelayObservation::SessionOpened {
                                native_session_id: "policy-native".into(),
                                resumed: false,
                                native_continuity_lost: false,
                                replaced_unused_native_session_id: None,
                            })
                            .unwrap();
                    }
                }
                relay
            });
            let complete_clear = PathBuf::from(&root).join("complete-clear");
            if std::env::var_os(CLEAR_MAILBOX_CONTROL).is_some() && complete_clear.exists() {
                std::fs::remove_file(complete_clear).unwrap();
                relay
                    .record_command_completed(
                        "clear-request",
                        mj_core::relay::RelayCommandOutcome::ContextCleared {
                            native_session_id: "replacement".into(),
                            memory: None,
                        },
                    )
                    .unwrap();
            }
            let starts_clear = matches!(
                &request.request,
                RelayRequest::Submit {
                    command: RelayCommand::ClearContext,
                    ..
                }
            );
            // This fake edits the state below, after the relay digested it, so
            // it cannot vouch for a digest: every attach gets the whole state.
            let mut request = request;
            if preparation.is_some()
                && let RelayRequest::Attach {
                    known_state_digest, ..
                } = &mut request.request
            {
                *known_state_digest = None;
            }
            let mut response =
                if let mj_core::relay::RelayRequest::Reviewer { role, .. } = &request.request {
                    if let Some(marker) = &marker
                        && role.as_deref() == Some("slow")
                    {
                        std::fs::write(marker, b"started").unwrap();
                        std::io::copy(&mut input, &mut std::io::sink()).unwrap();
                        std::fs::write(marker, b"disconnected").unwrap();
                        return;
                    }
                    mj_core::relay::RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        body: mj_core::relay::RelayResponseBody::Ok {
                            payload: mj_core::relay::RelayResponsePayload::ReviewBaselineAdvanced,
                        },
                    }
                } else if let RelayRequest::SetOperatorAway { away, revision } = &request.request {
                    use std::io::Write as _;
                    let mut log = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(PathBuf::from(&root).join("operator-presence.log"))
                        .unwrap();
                    writeln!(log, "{revision} {away}").unwrap();
                    mj_core::relay::RelayResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        body: RelayResponseBody::Ok {
                            payload: RelayResponsePayload::Status(relay.operational_state()),
                        },
                    }
                } else {
                    let wire_protocol = request.protocol_version;
                    let mut request = request;
                    if memory_protocol.is_some() {
                        // This scoped peer speaks its advertised wire version,
                        // while the in-process relay retains its strict current protocol.
                        request.protocol_version = mj_core::relay::RELAY_PROTOCOL_VERSION;
                    }
                    let mut response = relay.handle(request);
                    if memory_protocol.is_some() {
                        response.protocol_version = wire_protocol;
                    }
                    response
                };
            if starts_clear && std::env::var_os(CLEAR_MAILBOX_CONTROL).is_some() {
                relay.claim_pending_commands(true).unwrap();
            }
            if let Some(preparation) = preparation {
                relay.dispatch_preparation_lifecycle().unwrap();
                let state = match &mut response.body {
                    RelayResponseBody::Ok {
                        payload: RelayResponsePayload::Status(state),
                    } => Some(state),
                    RelayResponseBody::Ok {
                        payload:
                            RelayResponsePayload::Attached {
                                state,
                                state_digest,
                                ..
                            },
                    } => {
                        *state_digest = None;
                        Some(state)
                    }
                    _ => None,
                };
                if let Some(state) = state {
                    state.harness_preparation = Some(preparation);
                    if let Ok(shell) =
                        std::fs::read(PathBuf::from(&root).join("active-user-shell.json"))
                    {
                        state.active_user_shells = vec![serde_json::from_slice(&shell).unwrap()];
                    }
                }
            }
            mj_core::relay::write_relay_frame(&mut output, &response).unwrap();
        }
    }
}

fn policy_test_options() -> Vec<agent_client_protocol::v1::schema::SessionConfigOption> {
    use agent_client_protocol::v1::schema::*;
    vec![
        SessionConfigOption::select(
            "provider-model",
            "Model",
            "current",
            SessionConfigSelectOptions::Ungrouped(
                ["current", "allowed", "hidden"]
                    .map(|value| SessionConfigSelectOption::new(value, value))
                    .to_vec(),
            ),
        )
        .category(SessionConfigOptionCategory::Model),
    ]
}

/// The source relay journal an in-place move starts from.
pub fn seed_source_relay(worker_root: &Path) -> PathBuf {
    let source_relay = worker_root.parent().unwrap().join("source-relay");
    std::fs::create_dir_all(&source_relay).unwrap();
    let mut seed =
        crate::relay::DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
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
    source_relay
}

//! Exercise native recovery through a real, disposable worker process.
#![cfg(unix)]

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, TextContent};
use mj_core::relay::{
    RELAY_EVENT_GENESIS_DIGEST, RELAY_PROTOCOL_VERSION, RelayCommand, RelayCommandOutcome,
    RelayObservation, RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponsePayload,
};
use mj_core::targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
use mj_worker::relay::DurableRelay;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

fn recovered_prompt_finished(root: &std::path::Path) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(root.join("control.sock")) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let request = RelayRequestEnvelope {
        request_id: "recovery-status".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Status,
    };
    serde_json::to_writer(&mut stream, &request).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    if std::io::BufReader::new(stream)
        .read_line(&mut line)
        .is_err()
    {
        return false;
    }
    let Ok(response) = serde_json::from_str::<mj_core::relay::RelayResponseEnvelope>(&line) else {
        return false;
    };
    matches!(response.body, RelayResponseBody::Ok {
        payload: RelayResponsePayload::Status(state)
    } if state.native_session_id.as_deref() == Some("replacement")
        && state.active_prompt.is_none() && state.queued_prompts.is_empty())
}

/// Where the worker's relay state comes from before it starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayOrigin {
    /// A previous worker's journal, which opened the native session itself.
    Journal,
    /// A checkpoint restore: only the relay seed exists, and the native
    /// identity arrives with the launch configuration.
    RestoredSeed,
}

fn recover_missing_native_session(harness: &str, missing_error: &str, used: bool) {
    recover_missing_native_session_from(RelayOrigin::Journal, harness, missing_error, used);
}

fn recover_missing_native_session_from(
    origin: RelayOrigin,
    harness: &str,
    missing_error: &str,
    used: bool,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker");
    let log = temp.path().join("bridge.log");
    let script = temp.path().join("bridge.py");
    std::fs::write(
        &script,
        format!(
            r#"import json, sys
log = {log}
error = {error}
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    with open(log, "a") as output:
        output.write(method + "\n")
    ident = request.get("id")
    if method == "initialize":
        result = {{"protocolVersion": 1, "agentCapabilities": {{"loadSession": True}}}}
    elif method in ("session/load", "session/resume"):
        print(json.dumps({{"jsonrpc": "2.0", "id": ident,
                          "error": {{"code": -32603, "message": error}}}}), flush=True)
        continue
    elif method == "session/new":
        result = {{"sessionId": "replacement", "modes": {{"currentModeId": "agent",
                  "availableModes": [{{"id": "agent", "name": "Guardian"}},
                                     {{"id": "auto", "name": "Auto"}}]}}}}
    elif method == "session/prompt":
        # Answer the way a real agent does: the reply streams as an update on
        # the session the prompt named, then the prompt returns (R8-1).
        print(json.dumps({{"jsonrpc": "2.0", "method": "session/update", "params": {{
            "sessionId": request["params"]["sessionId"],
            "update": {{"sessionUpdate": "agent_message_chunk",
                        "content": {{"type": "text", "text": "recovered reply"}}}}}}}}),
              flush=True)
        result = {{"stopReason": "end_turn"}}
    else:
        result = {{}}
    if ident is not None:
        print(json.dumps({{"jsonrpc": "2.0", "id": ident, "result": result}}), flush=True)
"#,
            log = serde_json::to_string(&log).unwrap(),
            error = serde_json::to_string(missing_error).unwrap(),
        ),
    )
    .unwrap();

    match origin {
        RelayOrigin::Journal => {
            let mut relay = DurableRelay::open(&root, SESSION_ID, "prior-worker").unwrap();
            relay
                .record_observation(RelayObservation::SessionOpened {
                    native_session_id: "missing-thread".into(),
                    resumed: false,
                    native_continuity_lost: false,
                    replaced_unused_native_session_id: None,
                })
                .unwrap();
            let accepted = relay.handle(RelayRequestEnvelope {
                request_id: "submit-queued-prompt".into(),
                protocol_version: RELAY_PROTOCOL_VERSION,
                request: RelayRequest::Submit {
                    command_id: "queued-prompt".into(),
                    command: RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new("do the queued work"))],
                    },
                },
            });
            assert!(matches!(
                accepted.body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::Accepted { .. }
                }
            ));
            if used {
                relay.mark_native_session_used().unwrap();
            }
            assert_eq!(relay.native_session_may_have_history(), used);
        }
        RelayOrigin::RestoredSeed => {
            // What `restore_checkpoint` leaves for a session suspended with
            // one prompt still queued and none ever sent.
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                mj_core::relay::restored_relay_seed_path(&root),
                serde_json::to_vec(&mj_core::relay::RestoredRelaySeed {
                    event_frontier: 4,
                    event_frontier_digest: "c".repeat(64),
                    queued_prompts: vec![mj_core::archive::CanonicalQueuedPrompt {
                        command_id: "queued-prompt".into(),
                        kind: mj_core::archive::CanonicalQueuedCommandKind::Prompt,
                        content: vec![serde_json::json!({
                            "type": "text", "text": "do the queued work"
                        })],
                        queued_at_ms: 1,
                    }],
                    accepted_config: Default::default(),
                    native_session_unused: !used,
                })
                .unwrap(),
            )
            .unwrap();
        }
    }

    let config = temp.path().join("launch.json");
    let mut launch = serde_json::json!({
        "session_id": SESSION_ID, "harness": harness,
        "bridge_command": "python3", "bridge_args": [script],
        "environment": {}, "target_environment": {"MJ_INSTANCE": "qa-empty-recovery-1063"},
        "cwd": temp.path(), "execution_policy": "configured_approvals"
    });
    if origin == RelayOrigin::RestoredSeed {
        launch["native_session_id"] = serde_json::json!("missing-thread");
    }
    std::fs::write(&config, serde_json::to_vec(&launch).unwrap()).unwrap();
    let mut command = CommandSpec::new(
        env!("CARGO_BIN_EXE_mj-worker"),
        [
            "worker",
            "run",
            "--root",
            root.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
    );
    command
        .env
        .insert("MJ_INSTANCE".into(), "wrong-launcher-instance".into());
    let worker = std::thread::spawn(move || {
        BoundedProcessExecutor::new(Duration::from_secs(20)).execute(&command)
    });

    let observation = (|| -> anyhow::Result<String> {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let methods = std::fs::read_to_string(&log).unwrap_or_default();
            if used && methods.contains("session/load") && worker.is_finished() {
                return Ok(methods);
            }
            if !used && methods.contains("session/prompt") && recovered_prompt_finished(&root) {
                return Ok(methods);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "bridge never received queued work: {methods}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    let stop = mj_controller::targets::stop_worker_daemon_script(root.to_str().unwrap());
    let stopped = BoundedProcessExecutor::new(Duration::from_secs(8))
        .execute(&CommandSpec::new("sh", ["-c", &stop]));
    let owner = worker.join().expect("worker owner panicked");
    let methods = observation.unwrap_or_else(|error| {
        panic!(
            "{error:#}; worker: {}; stop: {stopped:?}",
            owner
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
                .unwrap_or_else(|error| error.to_string())
        )
    });
    assert_eq!(stopped.unwrap().status, 0);
    let output = owner.expect("worker did not stop within its deadline");
    assert!(
        methods.contains("session/load") || methods.contains("session/resume"),
        "{methods}"
    );
    if used {
        assert_ne!(output.status, 0);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("has no native history"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!methods.contains("session/new"), "{methods}");
        if origin == RelayOrigin::Journal {
            let relay = DurableRelay::open(&root, SESSION_ID, "recovered-worker").unwrap();
            assert_eq!(
                relay.operational_state().native_session_id.as_deref(),
                Some("missing-thread")
            );
        }
        return;
    }
    assert_eq!(methods.matches("session/new").count(), 1, "{methods}");
    assert_eq!(methods.matches("session/prompt").count(), 1, "{methods}");

    let relay = DurableRelay::open(&root, SESSION_ID, "recovered-worker").unwrap();
    let restored_frontier = "c".repeat(64);
    let (after, digest) = match origin {
        RelayOrigin::Journal => (0, RELAY_EVENT_GENESIS_DIGEST),
        RelayOrigin::RestoredSeed => (4, restored_frontier.as_str()),
    };
    let events = relay.events_after(after, digest).unwrap();
    // The opening names the session it replaced and why it could: the
    // controller accepts a new identity on resume only on this evidence (R7-5).
    assert!(
        events.iter().any(|event| matches!(
            &event.observation,
            RelayObservation::SessionOpened {
                native_session_id,
                resumed: false,
                replaced_unused_native_session_id: Some(replaced),
                ..
            } if native_session_id == "replacement" && replaced == "missing-thread"
        )),
        "{events:#?}"
    );
    assert_eq!(
        relay
            .operational_state()
            .replaced_unused_native_session_id
            .as_deref(),
        Some("missing-thread")
    );
    assert!(events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::Warning { message } if message.contains("new empty session")
    )));
    // The replacement session is live, not a replay: the reply reaches the
    // journal and the turn ends answered rather than `prompt_unanswered` (R8-1).
    assert!(
        events.iter().any(|event| matches!(
            &event.observation,
            RelayObservation::SessionUpdate { update } if matches!(
                update.as_ref(),
                SessionUpdate::AgentMessageChunk(chunk) if matches!(
                    &chunk.content,
                    ContentBlock::Text(text) if text.text == "recovered reply"
                )
            )
        )),
        "{events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            &event.observation,
            RelayObservation::CommandCompleted {
                command_id,
                outcome: RelayCommandOutcome::Prompt { stop_reason, .. },
            } if command_id == "queued-prompt" && stop_reason == "EndTurn"
        )),
        "{events:#?}"
    );
}

#[test]
fn codex_missing_unused_thread_recovers_after_worker_restart() {
    recover_missing_native_session(
        "codex",
        r#"Internal error: {"details": "no rollout found for thread id missing-thread"}"#,
        false,
    );
}

#[test]
fn claude_missing_unused_session_recovers_after_worker_restart() {
    recover_missing_native_session(
        "claude",
        "Resource not found: missing-thread: {\n  \"uri\": \"missing-thread\"\n}",
        false,
    );
}

#[test]
fn used_native_history_is_not_replaced_after_worker_restart() {
    recover_missing_native_session(
        "codex",
        r#"Internal error: {"details": "no rollout found for thread id missing-thread"}"#,
        true,
    );
    recover_missing_native_session(
        "claude",
        "Resource not found: missing-thread: {\n  \"uri\": \"missing-thread\"\n}",
        true,
    );
}

/// I2-7: resume after suspending a session that was never prompted. The
/// worker starts from the checkpoint's relay seed with the native identity in
/// its launch configuration, and the harness has no record of the session.
/// Codex's wording is the one R4 recorded ("thread not found: <id>").
#[test]
fn a_never_prompted_session_resumes_fresh_from_its_checkpoint() {
    recover_missing_native_session_from(
        RelayOrigin::RestoredSeed,
        "codex",
        r#"Internal error: {"details": "thread not found: missing-thread"}"#,
        false,
    );
    recover_missing_native_session_from(
        RelayOrigin::RestoredSeed,
        "claude",
        "Resource not found: missing-thread: {\n  \"uri\": \"missing-thread\"\n}",
        false,
    );
    // A checkpoint that cannot vouch for the session keeps it protected.
    recover_missing_native_session_from(
        RelayOrigin::RestoredSeed,
        "codex",
        r#"Internal error: {"details": "thread not found: missing-thread"}"#,
        true,
    );
}

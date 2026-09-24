//! Exercise native recovery through a real, disposable worker process.
#![cfg(unix)]

use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use mj_core::relay::{
    RELAY_EVENT_GENESIS_DIGEST, RELAY_PROTOCOL_VERSION, RelayCommand, RelayObservation,
    RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponsePayload,
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

fn recover_missing_native_session(harness: &str, missing_error: &str, used: bool) {
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

    let mut relay = DurableRelay::open(&root, SESSION_ID, "prior-worker").unwrap();
    relay
        .record_observation(RelayObservation::SessionOpened {
            native_session_id: "missing-thread".into(),
            resumed: false,
            native_continuity_lost: false,
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
    drop(relay);

    let config = temp.path().join("launch.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "session_id": SESSION_ID, "harness": harness,
            "bridge_command": "python3", "bridge_args": [script],
            "environment": {}, "target_environment": {"MJ_INSTANCE": "qa-empty-recovery-1063"},
            "cwd": temp.path(), "execution_policy": "configured_approvals"
        }))
        .unwrap(),
    )
    .unwrap();
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
        let relay = DurableRelay::open(&root, SESSION_ID, "recovered-worker").unwrap();
        assert_eq!(
            relay.operational_state().native_session_id.as_deref(),
            Some("missing-thread")
        );
        return;
    }
    assert_eq!(methods.matches("session/new").count(), 1, "{methods}");
    assert_eq!(methods.matches("session/prompt").count(), 1, "{methods}");

    let relay = DurableRelay::open(&root, SESSION_ID, "recovered-worker").unwrap();
    let events = relay.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::SessionOpened { native_session_id, .. }
            if native_session_id == "replacement"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::Warning { message } if message.contains("new empty session")
    )));
    assert!(events.iter().any(|event| matches!(
        &event.observation,
        RelayObservation::CommandCompleted { command_id, .. } if command_id == "queued-prompt"
    )));
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

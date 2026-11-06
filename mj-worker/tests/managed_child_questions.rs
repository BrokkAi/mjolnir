//! A managed child's permission request must reach its parent, not wait for a person.
#![cfg(unix)]

use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
use mj_core::relay::{
    RELAY_PROTOCOL_VERSION, RelayCommand, RelayRequest, RelayRequestEnvelope, RelayResponseBody,
    RelayResponsePayload,
};
use mj_core::targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
use mj_worker::relay::DurableRelay;
use std::time::{Duration, Instant};

#[path = "../src/test_support/timeouts.rs"]
mod timeouts;
use timeouts::IO_TIMEOUT;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

/// A real worker process reads the launch file as a daemon writes it. The
/// scripted bridge asks permission during its prompt and records the answer
/// it receives; a form left open for a person would leave it waiting.
// Hard-won: #1451: the worker dropped the managed-child marker before building the ACP launch, so a child's permission form stayed open.
#[test]
fn a_managed_child_permission_request_is_cancelled_by_its_worker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker");
    let answer_log = temp.path().join("permission-answer.json");
    let script = temp.path().join("bridge.py");
    std::fs::write(
        &script,
        format!(
            r#"import json, sys
answer_log = {answer_log}
def send(message):
    print(json.dumps(message), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        result = {{"protocolVersion": 1, "agentCapabilities": {{}}}}
    elif method == "session/new":
        result = {{"sessionId": "child-session", "modes": {{"currentModeId": "agent",
                  "availableModes": [{{"id": "agent", "name": "Guardian"}}]}}}}
    elif method == "session/prompt":
        send({{"jsonrpc": "2.0", "id": "permission-1", "method": "session/request_permission",
              "params": {{"sessionId": "child-session",
                         "toolCall": {{"toolCallId": "call-1", "title": "Bash", "kind": "execute"}},
                         "options": [
                             {{"optionId": "allow", "name": "Allow", "kind": "allow_once"}},
                             {{"optionId": "reject", "name": "Reject", "kind": "reject_once"}}]}}}})
        # The answer arrives on stdin before the prompt can finish.
        answer = json.loads(sys.stdin.readline())
        with open(answer_log, "w") as output:
            json.dump(answer, output)
        result = {{"stopReason": "end_turn"}}
    else:
        result = {{}}
    if ident is not None:
        send({{"jsonrpc": "2.0", "id": ident, "result": result}})
"#,
            answer_log = serde_json::to_string(&answer_log).unwrap(),
        ),
    )
    .unwrap();

    let mut relay = DurableRelay::open(&root, SESSION_ID, "prior-worker").unwrap();
    let accepted = relay.handle(RelayRequestEnvelope {
        request_id: "submit-prompt".into(),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::Submit {
            command_id: "child-prompt".into(),
            command: RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("run a command"))],
            },
        },
    });
    assert!(matches!(
        accepted.body,
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Accepted { .. }
        }
    ));
    drop(relay);

    let harness_home = temp.path().join("harness-home");
    std::fs::create_dir_all(&harness_home).unwrap();
    let config = temp.path().join("launch.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "session_id": SESSION_ID, "harness": "codex",
            "bridge_command": "python3", "bridge_args": [script],
            "environment": {"MJ_SESSION_MANAGED_SUBAGENT": "1"},
            "target_environment": {"MJ_INSTANCE": "qa-managed-child-1451"},
            "cwd": temp.path(), "execution_policy": "configured_approvals",
            "handback_tool": true, "harness_home": harness_home
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
    let worker =
        std::thread::spawn(move || BoundedProcessExecutor::new(IO_TIMEOUT * 2).execute(&command));

    let observation = (|| -> anyhow::Result<serde_json::Value> {
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            if let Ok(bytes) = std::fs::read(&answer_log)
                && let Ok(answer) = serde_json::from_slice(&bytes)
            {
                return Ok(answer);
            }
            anyhow::ensure!(
                !worker.is_finished(),
                "worker exited before the bridge received an answer"
            );
            anyhow::ensure!(
                Instant::now() < deadline,
                "the permission request stayed open: the worker is not treating this session as a managed child"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    let stop = mj_controller::targets::stop_worker_daemon_script(root.to_str().unwrap());
    let stopped =
        BoundedProcessExecutor::new(IO_TIMEOUT).execute(&CommandSpec::new("sh", ["-c", &stop]));
    let owner = worker.join().expect("worker owner panicked");
    let worker_log = std::fs::read_to_string(root.join("worker.log")).unwrap_or_default();
    let answer = observation.unwrap_or_else(|error| {
        panic!(
            "{error:#}; worker: {}; worker log: {worker_log}; stop: {stopped:?}",
            owner
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
                .unwrap_or_else(|error| error.to_string())
        )
    });
    assert_eq!(answer["id"], "permission-1", "{answer}");
    assert_eq!(
        answer["result"]["outcome"]["outcome"], "cancelled",
        "{answer}"
    );
}

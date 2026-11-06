//! A managed child's questions go to its parent; its approvals stay with a person.
#![cfg(unix)]

use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
use mj_core::elicitation::{ElicitationResponse, ElicitationValue};
use mj_core::relay::{
    RELAY_PROTOCOL_VERSION, RelayCommand, RelayRequest, RelayRequestEnvelope, RelayResponseBody,
    RelayResponseEnvelope, RelayResponsePayload,
};
use mj_core::targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
use mj_worker::relay::DurableRelay;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

#[path = "../src/test_support/timeouts.rs"]
mod timeouts;
use timeouts::IO_TIMEOUT;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

/// Answer a form through the worker's control socket, as the daemon does
/// for a person. False until the worker holds a form with this id.
fn answer_form(root: &std::path::Path, elicitation_id: &str, choice: &str) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(root.join("control.sock")) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let request = RelayRequestEnvelope {
        request_id: format!("answer-{elicitation_id}"),
        protocol_version: RELAY_PROTOCOL_VERSION,
        request: RelayRequest::RespondElicitation {
            elicitation_id: elicitation_id.into(),
            response: ElicitationResponse::Accept {
                content: [("choice".into(), ElicitationValue::String(choice.into()))].into(),
            },
        },
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
    serde_json::from_str::<RelayResponseEnvelope>(&line)
        .is_ok_and(|response| matches!(response.body, RelayResponseBody::Ok { .. }))
}

/// A real worker process reads the launch file as a daemon writes it. The
/// scripted bridge asks a question and then a tool permission during one
/// prompt, and records the answer to each. The question comes back
/// cancelled at once, which only a worker that knows it is a managed child
/// does. The permission waits, and only a person's answer selects "allow".
// Hard-won: #1451: the worker dropped the managed-child marker before building the ACP launch, and the parent could not grant a child's permission.
#[test]
fn a_managed_child_sends_questions_to_its_parent_and_keeps_approvals_for_a_person() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker");
    let answers = temp.path().join("answers.jsonl");
    let script = temp.path().join("bridge.py");
    std::fs::write(
        &script,
        format!(
            r#"import json, sys
answers = {answers}
def send(message):
    print(json.dumps(message), flush=True)
def ask(ident, method, params):
    send({{"jsonrpc": "2.0", "id": ident, "method": method, "params": params}})
    # The answer arrives on stdin before the prompt can finish.
    for line in sys.stdin:
        answer = json.loads(line)
        if answer.get("id") == ident:
            with open(answers, "a") as output:
                output.write(json.dumps(answer) + "\n")
            return
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
        ask("question-1", "elicitation/create", {{
            "sessionId": "child-session", "mode": "form", "message": "Which file?",
            "requestedSchema": {{"type": "object", "properties": {{
                "file": {{"type": "string", "title": "File"}}}}}}}})
        ask("permission-1", "session/request_permission", {{
            "sessionId": "child-session",
            "toolCall": {{"toolCallId": "call-1", "title": "Bash", "kind": "execute"}},
            "options": [
                {{"optionId": "allow", "name": "Allow", "kind": "allow_once"}},
                {{"optionId": "reject", "name": "Reject", "kind": "reject_once"}}]}})
        result = {{"stopReason": "end_turn"}}
    else:
        result = {{}}
    if ident is not None:
        send({{"jsonrpc": "2.0", "id": ident, "result": result}})
"#,
            answers = serde_json::to_string(&answers).unwrap(),
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

    let recorded = || -> Vec<serde_json::Value> {
        std::fs::read_to_string(&answers)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    };
    let observation = (|| -> anyhow::Result<Vec<serde_json::Value>> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut answered = false;
        loop {
            let seen = recorded();
            if seen.len() == 2 {
                return Ok(seen);
            }
            // Answer only after the question's cancellation reached the
            // bridge, so the permission cannot have been answered by anyone
            // but this simulated person.
            if seen.len() == 1 && !answered {
                answered = answer_form(&root, "tool-permission-1", "allow");
            }
            anyhow::ensure!(
                !worker.is_finished(),
                "worker exited before the bridge received both answers: {seen:?}"
            );
            anyhow::ensure!(
                Instant::now() < deadline,
                "the bridge did not receive both answers: {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    let stop = mj_controller::targets::stop_worker_daemon_script(root.to_str().unwrap());
    let stopped =
        BoundedProcessExecutor::new(IO_TIMEOUT).execute(&CommandSpec::new("sh", ["-c", &stop]));
    let owner = worker.join().expect("worker owner panicked");
    let worker_log = std::fs::read_to_string(root.join("worker.log")).unwrap_or_default();
    let seen = observation.unwrap_or_else(|error| {
        panic!(
            "{error:#}; worker: {}; worker log: {worker_log}; stop: {stopped:?}",
            owner
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
                .unwrap_or_else(|error| error.to_string())
        )
    });
    assert_eq!(seen[0]["id"], "question-1", "{seen:?}");
    assert_eq!(
        seen[0]["result"]["action"], "cancel",
        "a managed child's question goes to its parent: {seen:?}"
    );
    assert_eq!(seen[1]["id"], "permission-1", "{seen:?}");
    assert_eq!(
        seen[1]["result"]["outcome"]["optionId"], "allow",
        "a person answers a managed child's permission: {seen:?}"
    );
}

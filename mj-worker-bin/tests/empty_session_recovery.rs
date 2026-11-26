//! Exercise native recovery through a real, disposable worker process.
#![cfg(unix)]

mj_core::enable_test_fixtures_on_startup!();

use agent_client_protocol::v1::schema::{ContentBlock, SessionUpdate, TextContent};
use mj_core::relay::{
    RELAY_EVENT_GENESIS_DIGEST, RELAY_PROTOCOL_VERSION, RelayCommand, RelayCommandOutcome,
    RelayObservation, RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponsePayload,
};
use mj_core::targets::{BoundedProcessExecutor, CommandExecutor, CommandSpec};
use mj_worker::relay::DurableRelay;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

#[path = "../../mj-worker/src/test_support/timeouts.rs"]
mod timeouts;
use timeouts::IO_TIMEOUT;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

fn recovered_prompt_finished(root: &std::path::Path, session_id: &str) -> bool {
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
    } if state.native_session_id.as_deref() == Some(session_id)
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

/// Codex's answer for a thread it never wrote: codex-acp wraps the reason in
/// an internal error.
fn codex_missing_thread(details: &str) -> serde_json::Value {
    serde_json::json!({"code": -32603, "message": "Internal error", "data": {"details": details}})
}

/// claude-agent-acp 0.99's answer for a session with no transcript on disk.
fn claude_missing_session() -> serde_json::Value {
    serde_json::json!({
        "code": -32002,
        "message": "Resource not found",
        "data": {"sessionId": "missing-thread"}
    })
}

fn recover_missing_native_session(harness: &str, missing_error: serde_json::Value, used: bool) {
    recover_native_session_from(
        RelayOrigin::Journal,
        harness,
        Some(missing_error),
        used,
        None,
        None,
    );
}

/// The bridge loads the original thread unless given a JSON-RPC error.
fn recover_native_session_from(
    origin: RelayOrigin,
    harness: &str,
    missing_error: Option<serde_json::Value>,
    used: bool,
    prior_used_session: Option<&str>,
    prompt_context: Option<&str>,
) {
    let original = if missing_error.is_some() {
        "missing-thread"
    } else {
        "saved-thread"
    };
    let ready = if missing_error.is_some() {
        "replacement"
    } else {
        original
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker");
    let log = temp.path().join("bridge.log");
    let script = temp.path().join("bridge.py");
    std::fs::write(
        &script,
        format!(
            r#"import json, sys
log = {log}
error = json.loads({error})
context = {context}
modes = {{"currentModeId": "agent",
          "availableModes": [{{"id": "agent", "name": "Guardian"}},
                             {{"id": "auto", "name": "Auto"}}]}}
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    with open(log, "a") as output:
        output.write(method + "\n")
    ident = request.get("id")
    if method == "initialize":
        result = {{"protocolVersion": 1, "agentCapabilities": {{"loadSession": True}}}}
    elif method in ("session/load", "session/resume"):
        assert request["params"]["sessionId"] == {original:?}, request
        if error is not None:
            print(json.dumps({{"jsonrpc": "2.0", "id": ident,
                              "error": error}}), flush=True)
            continue
        result = {{"modes": modes}}
    elif method == "session/new":
        result = {{"sessionId": "replacement", "modes": modes}}
    elif method == "session/prompt":
        assert request["params"]["sessionId"] == {ready:?}, request
        if context is not None:
            assert context in json.dumps(request), request
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
            error = serde_json::to_string(&serde_json::to_string(&missing_error).unwrap()).unwrap(),
            context = match prompt_context {
                Some(text) => serde_json::to_string(text).unwrap(),
                None => "None".into(),
            },
        ),
    )
    .unwrap();

    match origin {
        RelayOrigin::Journal => {
            let mut relay = DurableRelay::open(&root, SESSION_ID, "prior-worker").unwrap();
            if let Some(prior) = prior_used_session {
                relay
                    .record_observation(RelayObservation::SessionOpened {
                        native_session_id: prior.into(),
                        resumed: false,
                        native_continuity_lost: false,
                        replaced_unused_native_session_id: None,
                    })
                    .unwrap();
                relay.mark_native_session_used().unwrap();
            }
            relay
                .record_observation(RelayObservation::SessionOpened {
                    native_session_id: original.into(),
                    resumed: false,
                    native_continuity_lost: false,
                    replaced_unused_native_session_id: None,
                })
                .unwrap();
            if let Some(text) = prompt_context {
                relay.install_prompt_context(text.to_owned()).unwrap();
            }
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
            assert_eq!(
                relay.native_session_may_have_history(),
                used && prior_used_session.is_none()
            );
        }
        RelayOrigin::RestoredSeed => {
            // What `restore_checkpoint` leaves for a session suspended with
            // one prompt still queued and none ever sent.
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                mj_core::relay::restored_relay_seed_path(&root),
                serde_json::to_vec(&mj_core::relay::RestoredRelaySeed {
                    command_ledger: None,
                    assessment_state: None,
                    notification_stack: None,
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
                    native_use_evidence: None,
                    pending_prompt_context: None,
                    mailbox: None,
                })
                .unwrap(),
            )
            .unwrap();
        }
    }

    let config = temp.path().join("launch.json");
    // Every launch config the controller writes names the harness home; a
    // Codex worker registers its delegation tools there before starting.
    let harness_home = temp.path().join("harness-home");
    std::fs::create_dir_all(&harness_home).unwrap();
    // Controller staging registers the top-level session's message-only
    // server in Claude's private profile before the worker resolves its
    // target-local socket path.
    std::fs::write(
        harness_home.join(".claude.json"),
        serde_json::to_vec(&serde_json::json!({
            "mcpServers": {
                "mj-agents": {
                    "type": "stdio",
                    "command": "hel",
                    "args": ["worker", "subagent-mcp", "--socket", "placeholder"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut launch = serde_json::json!({
        "session_id": SESSION_ID, "harness": harness,
        "bridge_command": "python3", "bridge_args": [script],
        "environment": {}, "target_environment": {"MJ_INSTANCE": "qa-empty-recovery-1063"},
        "cwd": temp.path(), "execution_policy": "configured_approvals",
        "harness_home": harness_home
    });
    if origin == RelayOrigin::RestoredSeed {
        launch["native_session_id"] = serde_json::json!(original);
    }
    std::fs::write(&config, serde_json::to_vec(&launch).unwrap()).unwrap();
    // Stage both the running worker and this build's lifecycle control worker.
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mj-worker"), root.join("hel")).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_mj-worker"),
        mj_targets::targets::control_worker_path(root.to_str().unwrap()),
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
    // Leave time for both the observation and the subsequent stop.
    let worker =
        std::thread::spawn(move || BoundedProcessExecutor::new(IO_TIMEOUT * 2).execute(&command));

    // Hard-won: fdd49cc1e: Startup under load exhausted the fixture budget before the bridge received work.
    let observation = (|| -> anyhow::Result<String> {
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            let methods = std::fs::read_to_string(&log).unwrap_or_default();
            if used && methods.contains("session/load") && worker.is_finished() {
                return Ok(methods);
            }
            if !used
                && methods.contains("session/prompt")
                && recovered_prompt_finished(&root, ready)
            {
                return Ok(methods);
            }
            anyhow::ensure!(
                !worker.is_finished(),
                "worker exited before recovering queued work: {methods}"
            );
            anyhow::ensure!(
                Instant::now() < deadline,
                "bridge never received queued work: {methods}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    let stop = mj_targets::targets::stop_worker_daemon_script(root.to_str().unwrap());
    let stopped = BoundedProcessExecutor::new(IO_TIMEOUT)
        .execute(&CommandSpec::new("sh", ["-c", stop.unstaged_script()]));
    let owner = worker.join().expect("worker owner panicked");
    let worker_log = std::fs::read_to_string(root.join("worker.log"))
        .expect("read the owning worker's diagnostics");
    let methods = observation.unwrap_or_else(|error| {
        panic!(
            "{error:#}; worker: {}; worker log: {worker_log}; stop: {stopped:?}",
            owner
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
                .unwrap_or_else(|error| error.to_string())
        )
    });
    assert_eq!(stopped.unwrap().status, 0);
    let output = owner.expect("worker did not stop within its deadline");
    let reloads = methods
        .lines()
        .filter(|method| matches!(*method, "session/load" | "session/resume"))
        .count();
    assert_eq!(
        reloads,
        usize::from(used || harness != "codex" || origin == RelayOrigin::RestoredSeed),
        "{methods}"
    );
    if used {
        assert_ne!(output.status, 0);
        assert!(worker_log.contains("has no native history"), "{worker_log}");
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
    assert_eq!(output.status, 0, "{worker_log}");
    assert_eq!(
        methods.matches("session/new").count(),
        usize::from(missing_error.is_some()),
        "{methods}"
    );
    assert_eq!(methods.matches("session/prompt").count(), 1, "{methods}");

    let relay = DurableRelay::open(&root, SESSION_ID, "recovered-worker").unwrap();
    let restored_frontier = "c".repeat(64);
    let (after, digest) = match origin {
        RelayOrigin::Journal => (0, RELAY_EVENT_GENESIS_DIGEST),
        RelayOrigin::RestoredSeed => (4, restored_frontier.as_str()),
    };
    let events = relay.events_after(after, digest).unwrap();
    let replaced = missing_error.as_ref().map(|_| original);
    // The controller accepts a new identity on resume only when the journal
    // names the unused thread that was replaced (R7-5).
    assert!(
        events.iter().any(|event| matches!(
            &event.observation,
            RelayObservation::SessionOpened {
                native_session_id,
                resumed,
                replaced_unused_native_session_id,
                ..
            } if native_session_id == ready
                && *resumed == replaced.is_none()
                && replaced_unused_native_session_id.as_deref() == replaced
        )),
        "{events:#?}"
    );
    assert_eq!(
        relay
            .operational_state()
            .replaced_unused_native_session_id
            .as_deref(),
        replaced
    );
    assert_eq!(
        events.iter().any(|event| matches!(
            &event.observation,
            RelayObservation::Warning { message } if message.contains("new empty session")
        )),
        replaced.is_some()
    );
    // The opened session is live: the reply reaches the
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
                ..
            } if command_id == "queued-prompt" && stop_reason == "EndTurn"
        )),
        "{events:#?}"
    );
}

#[test]
fn codex_missing_unused_thread_recovers_after_worker_restart() {
    recover_missing_native_session(
        "codex",
        codex_missing_thread("thread not loaded: missing-thread"),
        false,
    );
}

#[test]
fn claude_missing_unused_session_recovers_after_worker_restart() {
    recover_missing_native_session("claude", claude_missing_session(), false);
}

// Hard-won: #1701: a used prior native session made a fresh non-resumed open look like it had history.
#[test]
fn claude_fresh_session_after_used_identity_recovers_after_worker_restart() {
    recover_native_session_from(
        RelayOrigin::Journal,
        "claude",
        Some(claude_missing_session()),
        false,
        Some("old-antigravity"),
        Some("HANDOFF-1701"),
    );
}

#[test]
fn used_native_history_is_not_replaced_after_worker_restart() {
    recover_missing_native_session(
        "codex",
        codex_missing_thread("no rollout found for thread id missing-thread"),
        true,
    );
    recover_missing_native_session("claude", claude_missing_session(), true);
}

/// I2-7: resume after suspending a session that was never prompted. The
/// worker starts from the checkpoint's relay seed with the native identity in
/// its launch configuration, and the harness has no record of the session.
/// Codex's wording is the one R4 recorded ("thread not found: <id>").
// Hard-won: 0bb9439: a restored session with no prompts could not resume its unused native thread.
#[test]
fn a_never_prompted_session_resumes_fresh_from_its_checkpoint() {
    recover_native_session_from(
        RelayOrigin::RestoredSeed,
        "codex",
        Some(codex_missing_thread("thread not found: missing-thread")),
        false,
        None,
        None,
    );
    recover_native_session_from(
        RelayOrigin::RestoredSeed,
        "claude",
        Some(claude_missing_session()),
        false,
        None,
        None,
    );
    // A checkpoint that cannot vouch for the session keeps it protected.
    recover_native_session_from(
        RelayOrigin::RestoredSeed,
        "codex",
        Some(codex_missing_thread("thread not found: missing-thread")),
        true,
        None,
        None,
    );
}

// Hard-won: dc5d9891a: a restored session whose only turns were mailbox wakes was replaced although loadable
#[test]
fn codex_loads_a_restored_thread_marked_unused_by_the_seed() {
    recover_native_session_from(RelayOrigin::RestoredSeed, "codex", None, false, None, None);
}

#[test]
fn rejected_seeded_config_exits_with_startup_failure_before_the_brief() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("worker");
    let home = temp.path().join("harness-home");
    std::fs::create_dir(&home).unwrap();
    let log = temp.path().join("methods.log");
    let script = temp.path().join("bridge.py");
    std::fs::write(
        &script,
        r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    with open(sys.argv[1], "a") as log:
        log.write(method + "\n")
    if "id" not in request:
        continue
    response = {"jsonrpc": "2.0", "id": request["id"]}
    if method == "initialize":
        response["result"] = {"protocolVersion": 1}
    elif method == "session/new":
        response["result"] = {"sessionId": "test", "modes": {
            "currentModeId": "agent", "availableModes": [{"id": "agent", "name": "Agent"}]},
            "configOptions": [
            {"id": "model", "name": "Model", "type": "select", "currentValue": "default",
             "options": [{"value": "default", "name": "Default"},
                         {"value": "selected", "name": "Selected"}]}]}
    elif method == "session/set_config_option":
        assert request["params"]["value"] == "selected", request
        response["error"] = {"code": -32603, "message": "selected model refused"}
    else:
        response["result"] = {}
    print(json.dumps(response), flush=True)
"#,
    )
    .unwrap();
    let config = temp.path().join("launch.json");
    let launch = serde_json::json!({
        "session_id": SESSION_ID, "harness": "codex",
        "bridge_command": "python3", "bridge_args": [script, log],
        "environment": {}, "target_environment": {"MJ_INSTANCE": "qa-seeded-config-refusal"},
        "cwd": temp.path(), "execution_policy": "configured_approvals",
        "harness_home": home,
        "initial_commands": [
            {"command_id": "seed-model", "command": RelayCommand::SetConfig {
                key: "model".into(), value: "selected".into()
            }},
            {"command_id": "seed-brief", "command": RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new("do the work"))]
            }}
        ]
    });
    std::fs::write(&config, serde_json::to_vec(&launch).unwrap()).unwrap();

    let command = CommandSpec::new(
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
    // The first run exits through dispatch. The second finds the persisted
    // refusal while opening the relay and must never reach the bridge.
    for _ in 0..2 {
        let output = BoundedProcessExecutor::new(IO_TIMEOUT)
            .execute(&command)
            .unwrap();
        assert_ne!(output.status, 0, "{output:?}");
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(mj_core::relay::WORKER_EXIT_FILE)).unwrap(),
        )
        .unwrap();
        let failure = record["harness_startup_failure"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("missing typed startup failure: {record}"));
        assert!(failure.contains("selected model refused"), "{failure}");
        let methods = std::fs::read_to_string(&log).unwrap();
        assert_eq!(methods.matches("session/set_config_option").count(), 1);
        assert!(!methods.contains("session/prompt"), "{methods}");
    }
}

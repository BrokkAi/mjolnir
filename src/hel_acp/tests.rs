use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use super::*;
use agent_client_protocol::schema::v1::{
    SessionConfigSelectGroup, SessionConfigSelectOption, SessionConfigSelectOptions, ToolCallUpdate,
};

#[test]
fn only_updates_for_tool_calls_created_on_the_live_connection_are_relayed() {
    let live_tool_calls = Mutex::new(BTreeSet::new());
    let metadata_only = SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new("old-tool", ToolCallUpdateFields::default()).meta(
            serde_json::Map::from_iter([(
                "terminal_output_delta".into(),
                serde_json::json!({"data": "replayed output"}),
            )]),
        ),
    );
    assert!(!session_update_is_relay_visible(
        &metadata_only,
        &live_tool_calls,
        "session-1"
    ));

    let delayed = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "old-tool",
        ToolCallUpdateFields::new().title("updated"),
    ));
    assert!(!session_update_is_relay_visible(
        &delayed,
        &live_tool_calls,
        "session-1"
    ));

    let created = SessionUpdate::ToolCall(agent_client_protocol::schema::v1::ToolCall::new(
        "live-tool",
        "read",
    ));
    assert!(session_update_is_relay_visible(
        &created,
        &live_tool_calls,
        "session-1"
    ));
    let visible = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "live-tool",
        ToolCallUpdateFields::new().title("updated"),
    ));
    assert!(session_update_is_relay_visible(
        &visible,
        &live_tool_calls,
        "session-1"
    ));
}

#[test]
fn project_memory_mcp_honors_harness_delivery_and_claude_native_memory() {
    let mut spec = LaunchSpec {
        command: "/worker/hel".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: vec!["/workspace/api".into()],
        extra_mcp_servers: Vec::new(),
        project_memory: Some(ProjectMemoryLaunchConfig {
            project_key: "abc".into(),
            root: "/profile/projects/abc/memory".into(),
            baseline_root: "/profile/projects/abc/.hel-memory-baseline".into(),
            repository_roots: BTreeMap::from([
                ("app".into(), "/workspace/app".into()),
                ("api".into(), "/workspace/api".into()),
            ]),
            mcp_delivery: ProjectMemoryMcpDelivery::Acp,
        }),
        resume_session: None,
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let servers = project_memory_mcp(&spec);
    let [McpServer::Stdio(server)] = servers.as_slice() else {
        panic!("non-Claude sessions receive exactly one memory MCP server");
    };
    assert_eq!(server.name, "mj-project-memory");
    assert_eq!(server.command, Path::new("/worker/hel"));
    assert_eq!(
        server.args,
        [
            "worker",
            "memory-mcp",
            "--root",
            "/profile/projects/abc/memory"
        ]
    );
    assert!(
        !server
            .args
            .iter()
            .any(|argument| argument.contains("store")),
        "the model-facing service must not expose store selection"
    );

    spec.project_memory.as_mut().unwrap().mcp_delivery = ProjectMemoryMcpDelivery::HarnessProfile;
    assert!(project_memory_mcp(&spec).is_empty());
    spec.project_memory.as_mut().unwrap().mcp_delivery = ProjectMemoryMcpDelivery::Acp;

    let mut claude = spec;
    claude.harness = HarnessKind::Claude;
    assert!(project_memory_mcp(&claude).is_empty());
}

#[test]
fn claude_session_metadata_disables_sandbox_only_for_unconstrained_targets() {
    let mut spec = LaunchSpec {
        command: "claude-agent-acp".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: vec!["/workspace/api".into()],
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
    assert_eq!(
        meta.pointer("/claudeCode/options/sandbox/enabled"),
        Some(&serde_json::Value::Bool(false))
    );
    for request in [
        serde_json::to_value(new_session_request(&spec, true)).unwrap(),
        serde_json::to_value(new_session_request(&spec, false)).unwrap(),
        serde_json::to_value(load_session_request(&spec, SessionId::from("native"))).unwrap(),
    ] {
        assert_eq!(
            request.pointer("/_meta/claudeCode/options/sandbox/enabled"),
            Some(&serde_json::Value::Bool(false)),
            "{request}"
        );
        assert_eq!(
            request["additionalDirectories"],
            serde_json::json!(["/workspace/api"]),
            "{request}"
        );
    }

    spec.execution_policy = ExecutionPolicy::ConfiguredApprovals;
    assert_eq!(session_request_meta(&spec), None);
    assert!(
        serde_json::to_value(new_session_request(&spec, true))
            .unwrap()
            .get("_meta")
            .is_none()
    );
    spec.execution_policy = ExecutionPolicy::Unconstrained;
    spec.harness = HarnessKind::Codex;
    assert_eq!(session_request_meta(&spec), None);
}

#[test]
fn finds_modes_in_flat_and_grouped_options() {
    let flat =
        SessionConfigKind::Select(agent_client_protocol::schema::v1::SessionConfigSelect::new(
            "default",
            vec![SessionConfigSelectOption::new("auto", "Auto")],
        ));
    assert!(select_contains(&flat, "auto"));

    let grouped =
        SessionConfigKind::Select(agent_client_protocol::schema::v1::SessionConfigSelect::new(
            "default",
            SessionConfigSelectOptions::Grouped(vec![SessionConfigSelectGroup::new(
                "permissions",
                "Permissions",
                vec![SessionConfigSelectOption::new(
                    "bypassPermissions",
                    "Bypass",
                )],
            )]),
        ));
    assert!(select_contains(&grouped, "bypassPermissions"));
}

#[test]
fn advertised_choices_flatten_groups_and_follow_the_option_category() {
    let model = SessionConfigOption::select(
        "gpt_model",
        "Model",
        "fast",
        SessionConfigSelectOptions::Grouped(vec![
            SessionConfigSelectGroup::new(
                "hosted",
                "Hosted",
                vec![SessionConfigSelectOption::new("fast", "Fast")],
            ),
            SessionConfigSelectGroup::new(
                "local",
                "Local",
                vec![SessionConfigSelectOption::new("deep", "Deep").description("Slower, better")],
            ),
        ]),
    )
    .category(SessionConfigOptionCategory::Model);
    let effort = SessionConfigOption::select(
        "reasoning_effort",
        "Effort",
        "low",
        SessionConfigSelectOptions::Ungrouped(vec![
            SessionConfigSelectOption::new("low", "Low"),
            SessionConfigSelectOption::new("high", "High"),
        ]),
    );
    let options = vec![model, effort];

    // The option id is not "model", so only the category can find it.
    assert_eq!(
        session_config_choices(&options, "model"),
        vec![
            SessionConfigChoice {
                value: "fast".into(),
                name: "Fast".into(),
                description: None,
            },
            SessionConfigChoice {
                value: "deep".into(),
                name: "Deep".into(),
                description: Some("Slower, better".into()),
            },
        ]
    );
    assert_eq!(
        session_config_choices(&options, "effort")
            .into_iter()
            .map(|choice| choice.value)
            .collect::<Vec<_>>(),
        vec!["low", "high"]
    );
}

#[test]
fn an_option_the_harness_does_not_advertise_offers_no_choices() {
    assert!(session_config_choices(&[], "model").is_empty());
    assert!(session_config_choices(&[], "effort").is_empty());

    // A harness that advertises only a mode selector configures neither.
    let mode = SessionConfigOption::select(
        "interaction_mode",
        "Mode",
        "plan",
        SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new("plan", "Plan")]),
    )
    .category(SessionConfigOptionCategory::Mode);
    assert!(session_config_choices(std::slice::from_ref(&mode), "model").is_empty());
    assert!(session_config_choices(std::slice::from_ref(&mode), "effort").is_empty());
    assert_eq!(session_config_choices(&[mode], "mode").len(), 1);
}

#[test]
fn live_config_finds_model_and_anvil_reasoning_effort_separately() {
    let model = SessionConfigOption::select(
        "model",
        "Model",
        "gpt-5.6-sol",
        vec![SessionConfigSelectOption::new("gpt-5.6-sol", "Sol")],
    )
    .category(SessionConfigOptionCategory::Model);
    let effort = SessionConfigOption::select(
        "reasoning_effort",
        "Reasoning effort",
        "high",
        vec![SessionConfigSelectOption::new("high", "High")],
    )
    .category(SessionConfigOptionCategory::Model);
    let options = vec![model, effort];

    assert_eq!(
        find_session_config_option(&options, "model")
            .unwrap()
            .id
            .to_string(),
        "model"
    );
    assert_eq!(
        find_session_config_option(&options, "effort")
            .unwrap()
            .id
            .to_string(),
        "reasoning_effort"
    );
}

#[test]
fn permission_request_warning_explains_required_permission_modes() {
    assert!(UNEXPECTED_PERMISSION_REQUEST_WARNING.contains("misconfigured"));
    assert!(UNEXPECTED_PERMISSION_REQUEST_WARNING.contains("unconstrained"));
}

#[tokio::test]
async fn runtime_event_delivery_waits_for_bounded_channel_capacity() {
    let (events_tx, mut events_rx) = mpsc::channel(1);
    emit_runtime_event(
        &events_tx,
        RuntimeEvent::Warning {
            message: "first".into(),
        },
    )
    .await
    .unwrap();

    let blocked_tx = events_tx.clone();
    let blocked = tokio::spawn(async move {
        emit_runtime_event(
            &blocked_tx,
            RuntimeEvent::Warning {
                message: "second".into(),
            },
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(
        !blocked.is_finished(),
        "event producer bypassed bounded-channel backpressure"
    );

    assert!(matches!(
        events_rx.recv().await,
        Some(RuntimeEvent::Warning { message }) if message == "first"
    ));
    blocked.await.unwrap().unwrap();
    assert!(matches!(
        events_rx.recv().await,
        Some(RuntimeEvent::Warning { message }) if message == "second"
    ));
}

#[test]
fn adapter_chatter_never_becomes_error_context() {
    assert_eq!(
        actionable_stderr_tail(
            "Unexpected case: {\"type\":\"vcs_state_changed\"}\nUnexpected case: {\"type\":\"other\"}"
        ),
        None
    );
    assert_eq!(
        actionable_stderr_tail(
            "Unexpected case: {\"type\":\"vcs_state_changed\"}\nnode: out of memory\nUnexpected case: {\"type\":\"other\"}"
        ),
        Some("node: out of memory".to_owned())
    );
    assert_eq!(
        actionable_stderr_tail(
            "Got response to unknown request null\nGot response to unknown request null"
        ),
        None
    );
    assert_eq!(
        actionable_stderr_tail(
            "Got response to unknown request null\nACP protocol failed: runtime identity missing"
        ),
        Some("ACP protocol failed: runtime identity missing".to_owned())
    );
    assert_eq!(actionable_stderr_tail("   "), None);
}

#[test]
fn an_auth_required_prompt_failure_carries_the_credential_marker() {
    let auth = prompt_failure_warning(&agent_client_protocol::Error::auth_required());
    assert!(auth.contains("prompt failed"), "{auth}");
    assert!(crate::hel_credentials::auth_failure_signature(
        HarnessKind::Claude,
        &auth
    ));

    let other = prompt_failure_warning(&agent_client_protocol::Error::internal_error());
    assert!(other.contains("prompt failed"), "{other}");
    assert!(!crate::hel_credentials::auth_failure_signature(
        HarnessKind::Claude,
        &other
    ));
}

#[test]
fn only_non_cancelled_prompts_without_updates_need_an_empty_response_warning() {
    assert!(prompt_returned_without_updates(&StopReason::EndTurn, 7, 7));
    assert!(!prompt_returned_without_updates(&StopReason::EndTurn, 7, 8));
    assert!(!prompt_returned_without_updates(
        &StopReason::Cancelled,
        7,
        7
    ));
}

/// Answers `initialize` and `session/new`, then fails the first
/// `session/prompt` with a JSON-RPC error and completes the second.
async fn scripted_bridge(stream: tokio::io::DuplexStream) -> usize {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut prompts = 0_usize;
    while let Some(line) = lines.next_line().await.expect("read scripted bridge input") {
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
        let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match method {
            "initialize" => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}})
            }
            "session/new" => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "scripted"}})
            }
            "session/prompt" => {
                prompts += 1;
                if prompts == 1 {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32000, "message": "Authentication required"},
                    })
                } else {
                    if prompts == 3 {
                        let update = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": "scripted",
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": "answer"}
                                }
                            }
                        });
                        write
                            .write_all(format!("{update}\n").as_bytes())
                            .await
                            .expect("write scripted session update");
                    }
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})
                }
            }
            _ => continue,
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
    prompts
}

/// Answers `initialize` and `session/new`, then — while the prompt is in
/// flight — sends the client an ext request and publishes the answer as
/// soon as it arrives, so a silent client shows up as a timeout.
async fn ext_request_bridge(
    stream: tokio::io::DuplexStream,
    method: &'static str,
    answered: tokio::sync::oneshot::Sender<serde_json::Value>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut answered = Some(answered);
    while let Some(line) = lines.next_line().await.expect("read bridge input") {
        let message: serde_json::Value =
            serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
        if message.get("id").and_then(serde_json::Value::as_str) == Some("ext-1") {
            if let Some(answered) = answered.take() {
                let _ = answered.send(message);
            }
            continue;
        }
        let Some(request_method) = message.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match request_method {
            "initialize" => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}})
            }
            "session/new" => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "scripted"}})
            }
            // Ask the client to leave plan mode without answering the
            // prompt: the turn only ends once the client replies, which is
            // exactly the hang this guards against.
            "session/prompt" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": "ext-1",
                "method": method,
                "params": {
                    "sessionId": "scripted",
                    "toolCallId": "call-1",
                    "planContent": "1. do the thing",
                },
            }),
            _ => continue,
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn answer_to_ext_request(
    method: &'static str,
    execution_policy: ExecutionPolicy,
) -> serde_json::Value {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (answered_tx, answered_rx) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(ext_request_bridge(bridge_stream, method, answered_tx));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());

    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    // Drain events so a full channel can never be mistaken for silence.
    let events = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Grok,
        execution_policy,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "first".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("plan it"))],
        })
        .await
        .unwrap();

    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), answered_rx)
        .await
        .expect("Hel must answer every incoming request instead of leaving the agent waiting")
        .expect("the bridge must publish the answer");

    drop(request_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), driver).await;
    bridge.abort();
    events.abort();
    answer
}

async fn elicitation_bridge(
    stream: tokio::io::DuplexStream,
    initialized: oneshot::Sender<serde_json::Value>,
    answered: oneshot::Sender<serde_json::Value>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut initialized = Some(initialized);
    let mut answered = Some(answered);
    let mut prompt_id = None;
    while let Some(line) = lines.next_line().await.expect("read bridge input") {
        let message: serde_json::Value = serde_json::from_str(&line).expect("valid JSON-RPC");
        if message.get("id").and_then(serde_json::Value::as_str) == Some("ask-1") {
            if let Some(answered) = answered.take() {
                let _ = answered.send(message);
            }
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": prompt_id.take().expect("prompt id recorded"),
                "result": {"stopReason": "end_turn"},
            });
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("finish prompt");
            continue;
        }
        let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match method {
            "initialize" => {
                if let Some(initialized) = initialized.take() {
                    let _ = initialized.send(message.clone());
                }
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"protocolVersion": 1},
                })
            }
            "session/new" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"sessionId": "scripted"},
            }),
            "session/prompt" => {
                prompt_id = Some(id);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "ask-1",
                    "method": "elicitation/create",
                    "params": {
                        "sessionId": "scripted",
                        "toolCallId": "question-tool",
                        "mode": "form",
                        "message": "Choose an architecture",
                        "requestedSchema": {
                            "type": "object",
                            "required": ["architecture"],
                            "properties": {
                                "architecture": {
                                    "type": "string",
                                    "title": "Architecture",
                                    "oneOf": [
                                        {"const": "thin", "title": "Thin callers"},
                                        {"const": "dynamic", "title": "Dynamic matrix"}
                                    ]
                                }
                            }
                        }
                    }
                })
            }
            _ => continue,
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
}

#[tokio::test]
async fn form_elicitation_is_advertised_rendered_and_answered() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (initialized_tx, initialized_rx) = oneshot::channel();
    let (answered_tx, answered_rx) = oneshot::channel();
    let bridge = tokio::spawn(elicitation_bridge(
        bridge_stream,
        initialized_tx,
        answered_tx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    let initialized = tokio::time::timeout(Duration::from_secs(5), initialized_rx)
        .await
        .expect("runtime initializes")
        .expect("bridge observes initialization");
    assert!(initialized["params"]["clientCapabilities"]["elicitation"]["form"].is_object());

    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-1".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("plan it"))],
        })
        .await
        .unwrap();
    let request = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("elicitation arrives")
            .expect("runtime event channel stays open");
        if let RuntimeEvent::ElicitationRequested { request } = event {
            break request;
        }
    };
    assert_eq!(request.message, "Choose an architecture");
    assert_eq!(request.fields[0].title, "Architecture");
    let (resolved_tx, resolved_rx) = oneshot::channel();
    request_tx
        .send(CommandRequest::ResolveElicitation {
            elicitation_id: request.id,
            response: ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "architecture".into(),
                    crate::hel_elicitation::ElicitationValue::String("thin".into()),
                )]),
            },
            resolved: resolved_tx,
        })
        .await
        .unwrap();
    assert_eq!(resolved_rx.await.unwrap(), Ok(()));
    let answered = tokio::time::timeout(Duration::from_secs(5), answered_rx)
        .await
        .expect("bridge receives answer")
        .expect("answer is published");
    assert_eq!(answered["result"]["action"], "accept");
    assert_eq!(answered["result"]["content"]["architecture"], "thin");

    drop(request_tx);
    tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("runtime exits")
        .expect("runtime task does not panic")
        .expect("runtime exits cleanly");
    bridge.await.unwrap();
}

/// Modeled on the `_meta.modelState` a signed-in `grok agent stdio`
/// returns from `initialize`.
fn grok_model_meta() -> serde_json::Map<String, serde_json::Value> {
    let state = serde_json::json!({
        "currentModelId": "grok-4.6",
        "availableModels": [
            {
                "modelId": "grok-4.6",
                "name": "Grok 4.6",
                "description": "SpaceXAI's latest frontier model",
                "_meta": {
                    "totalContextTokens": 500_000,
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "high",
                    "reasoningEfforts": [
                        {"id": "xhigh", "value": "xhigh", "label": "Extra High Effort", "description": "Highest effort and reasoning level", "default": true},
                        {"id": "high", "value": "high", "label": "High Effort", "default": true},
                        {"id": "medium", "value": "medium", "label": "Medium Effort", "default": false},
                        {"id": "low", "value": "low", "label": "Low Effort", "default": false}
                    ]
                }
            },
            {
                "modelId": "grok-4.5",
                "name": "Grok 4.5",
                "_meta": {
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "high",
                    "reasoningEfforts": [
                        {"id": "high", "value": "high", "label": "High Effort", "default": true},
                        {"id": "low", "value": "low", "label": "Low Effort", "default": false}
                    ]
                }
            }
        ]
    });
    let mut meta = serde_json::Map::new();
    meta.insert("modelState".into(), state);
    meta
}

#[test]
fn grok_plan_review_answers_are_user_selected() {
    let review = normalized_plan_review(
        "plan-review-grok-1".into(),
        &serde_json::json!({"plan_content": "Do nothing"}),
    );
    let encoded = serde_json::to_value(&review).unwrap();
    assert_eq!(
        serde_json::from_value::<ElicitationRequest>(encoded).unwrap(),
        review,
        "normalized reviews must survive the durable relay journal"
    );
    assert_eq!(
        review.fields[1].custom_answer_for.as_deref(),
        Some("action")
    );
    assert_eq!(
        review.fields[1].custom_answer_option.as_deref(),
        Some("revise")
    );
    let mut content = BTreeMap::new();
    content.insert(
        PLAN_REVIEW_ACTION.into(),
        ElicitationValue::String("implement".into()),
    );
    assert_eq!(
        grok::plan_response(ElicitationResponse::Accept { content }),
        serde_json::json!({"outcome": "approved"})
    );
}

#[test]
fn supported_permission_plan_reviews_are_detected_and_mapped_to_native_options() {
    use agent_client_protocol::schema::v1::{
        PermissionOption, ToolCallUpdate, ToolCallUpdateFields,
    };

    let fixtures = [
        ("Implement this plan?", "IMPLEMENT_PLAN_OPTION_ID"),
        ("ExitPlanMode", "default"),
        ("Review plan", "plan_approve"),
    ];
    for (title, approval_id) in fixtures {
        let request = RequestPermissionRequest::new(
            "session-1",
            ToolCallUpdate::new(
                "tool-1",
                ToolCallUpdateFields::new()
                    .title(title.to_owned())
                    .raw_input(serde_json::json!({"plan": "Do the work"})),
            ),
            vec![
                PermissionOption::new(approval_id, "Approve", PermissionOptionKind::AllowOnce),
                PermissionOption::new("plan_revise", "Revise", PermissionOptionKind::RejectOnce),
            ],
        );
        assert!(is_plan_permission(&request), "fixture {title}");
        let review = normalized_plan_review(
            "plan-review-1".into(),
            &serde_json::to_value(&request).unwrap(),
        );
        assert!(review.message.contains("Do the work"));
        let mut content = BTreeMap::new();
        content.insert(
            PLAN_REVIEW_ACTION.into(),
            ElicitationValue::String("implement".into()),
        );
        let response = permission_plan_response(&request, ElicitationResponse::Accept { content });
        assert_eq!(
            serde_json::to_value(response).unwrap()["outcome"]["optionId"],
            approval_id
        );
    }
}

/// The exact `session/request_permission` Claude Code's ACP bridge sends when
/// ExitPlanMode fires: a `switch_mode` tool call carrying the plan and a
/// `planFilePath`, titled "Ready to code?", with generic permission-mode option
/// ids. Captured from a live worker.log. None of the title/option heuristics
/// match it, so the classifier must key on the tool kind and plan payload.
#[test]
fn claude_exit_plan_mode_request_is_detected_and_mapped() {
    let request: RequestPermissionRequest = serde_json::from_value(serde_json::json!({
        "sessionId": "7c4ce22d-9b3a-421b-b9f9-f2b8d3b73bb8",
        "toolCall": {
            "toolCallId": "toolu_01L8aaLyXndAiFoQM9yhpsRy",
            "kind": "switch_mode",
            "title": "Ready to code?",
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": "# Add --version flag\n\nDo the work."}
            }],
            "rawInput": {
                "plan": "# Add --version flag\n\nDo the work.",
                "planFilePath": "/home/jonathan/.claude/plans/add-a-version-flag.md"
            }
        },
        "options": [
            {"optionId": "bypassPermissions", "name": "Yes, and bypass permissions", "kind": "allow_always"},
            {"optionId": "auto", "name": "Yes, and use \"auto\" mode", "kind": "allow_always"},
            {"optionId": "acceptEdits", "name": "Yes, and auto-accept edits", "kind": "allow_always"},
            {"optionId": "default", "name": "Yes, and manually approve edits", "kind": "allow_once"},
            {"optionId": "plan", "name": "No, keep planning", "kind": "reject_once"}
        ]
    }))
    .expect("captured Claude ExitPlanMode payload deserializes");

    assert!(
        is_plan_permission(&request),
        "Claude's switch_mode request must be classified as a plan review"
    );

    let review = normalized_plan_review(
        "plan-review-1".into(),
        &serde_json::to_value(&request).unwrap(),
    );
    assert!(review.message.contains("Add --version flag"));

    // Guardian implementations use Auto rather than manual edit approvals.
    let mut implement = BTreeMap::new();
    implement.insert(
        PLAN_REVIEW_ACTION.into(),
        ElicitationValue::String("implement".into()),
    );
    let PlanPermissionAnswer::Native(approved) = policy_plan_permission_answer(
        &request,
        ElicitationResponse::Accept { content: implement },
        HarnessKind::Claude,
        ExecutionPolicy::ConfiguredApprovals,
    )
    .unwrap() else {
        panic!("Auto is offered by this bridge");
    };
    assert_eq!(
        serde_json::to_value(approved).unwrap()["outcome"]["optionId"],
        "auto"
    );

    // Declining keeps planning by selecting the reject option instead of
    // cancelling the turn.
    let mut keep = BTreeMap::new();
    keep.insert(
        PLAN_REVIEW_ACTION.into(),
        ElicitationValue::String("keep_planning".into()),
    );
    let declined =
        permission_plan_response(&request, ElicitationResponse::Accept { content: keep });
    assert_eq!(
        serde_json::to_value(declined).unwrap()["outcome"]["optionId"],
        "plan"
    );
}

#[tokio::test]
async fn an_unknown_client_request_is_answered_with_an_error_rather_than_silence() {
    let answer =
        answer_to_ext_request("_someone.example/unknown", ExecutionPolicy::Unconstrained).await;
    assert!(
        answer.get("result").is_none(),
        "an unimplemented request must not be answered with a result: {answer}"
    );
    assert_eq!(
        answer["error"]["code"], -32601,
        "expected a method-not-found error: {answer}"
    );
}

/// Answers `initialize` (with or without Grok Build's model catalogue) and
/// `session/new`, then records the request Hel sends for a config change.
async fn config_change_bridge(
    stream: tokio::io::DuplexStream,
    model_catalogue: bool,
    observed: tokio::sync::oneshot::Sender<serde_json::Value>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut observed = Some(observed);
    while let Some(line) = lines.next_line().await.expect("read bridge input") {
        let message: serde_json::Value =
            serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
        let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = message
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match method {
            "initialize" => {
                let mut result = serde_json::json!({"protocolVersion": 1});
                if model_catalogue {
                    result["_meta"] = serde_json::Value::Object(grok_model_meta());
                }
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            "session/new" => {
                let config_options = if model_catalogue {
                    serde_json::json!([{
                        "id": "verbosity",
                        "name": "Verbosity",
                        "type": "select",
                        "currentValue": "normal",
                        "options": [{"value": "normal", "name": "Normal"},
                                    {"value": "detailed", "name": "Detailed"}],
                    }])
                } else {
                    serde_json::json!([{
                        "id": "model",
                        "name": "Model",
                        "category": "model",
                        "type": "select",
                        "currentValue": "sonnet",
                        "options": [{"value": "sonnet", "name": "Sonnet"},
                                    {"value": "opus", "name": "Opus"}],
                    }])
                };
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "sessionId": "scripted",
                        "configOptions": config_options,
                    },
                })
            }
            _ => {
                if let Some(observed) = observed.take() {
                    let _ = observed.send(message.clone());
                }
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
            }
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn config_change_request(
    harness: HarnessKind,
    model_catalogue: bool,
    key: &str,
    value: &str,
) -> serde_json::Value {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(config_change_bridge(
        bridge_stream,
        model_catalogue,
        observed_tx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());

    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let events = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    request_tx
        .send(CommandRequest::SetConfig {
            request_id: "config-1".into(),
            key: key.to_owned(),
            value: value.to_owned(),
        })
        .await
        .unwrap();

    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), observed_rx)
        .await
        .expect("Hel must send a configuration request")
        .expect("the bridge must publish the request");

    drop(request_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), driver).await;
    bridge.abort();
    events.abort();
    observed
}

#[derive(Clone, Copy)]
enum ModeSurface {
    Legacy,
    Both,
}

async fn mode_change_bridge(
    stream: tokio::io::DuplexStream,
    surface: ModeSurface,
    observed: tokio::sync::oneshot::Sender<serde_json::Value>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mode_option = |current: &str| {
        serde_json::json!({
            "id": "interaction_mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": current,
            "options": [
                {"value": "default", "name": "Default"},
                {"value": "plan", "name": "Plan"},
                {"value": "agent", "name": "Agent"},
                {"value": "agent-full-access", "name": "Full access"}
            ]
        })
    };
    let modes = serde_json::json!({
        "currentModeId": "default",
            "availableModes": [
                {"id": "default", "name": "Default"},
                {"id": "plan", "name": "Plan"},
                {"id": "agent", "name": "Agent"},
                {"id": "agent-full-access", "name": "Full access"}
        ]
    });
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut observed = Some(observed);
    while let Some(line) = lines.next_line().await.expect("read bridge input") {
        let message: serde_json::Value = serde_json::from_str(&line).unwrap();
        let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned().unwrap_or_default();
        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}
            }),
            "session/new" => {
                let mut result = serde_json::json!({"sessionId": "scripted"});
                if matches!(surface, ModeSurface::Both) {
                    result["configOptions"] = serde_json::json!([mode_option("default")]);
                }
                if matches!(surface, ModeSurface::Legacy | ModeSurface::Both) {
                    result["modes"] = modes.clone();
                }
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            "session/set_config_option" => {
                if let Some(observed) = observed.take() {
                    let _ = observed.send(message.clone());
                }
                let selected = message["params"]["value"].as_str().unwrap_or("default");
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"configOptions": [mode_option(selected)]}
                })
            }
            _ => {
                if let Some(observed) = observed.take() {
                    let _ = observed.send(message.clone());
                }
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
            }
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn mode_change_request(surface: ModeSurface) -> serde_json::Value {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(mode_change_bridge(bridge_stream, surface, observed_tx));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let events = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    request_tx
        .send(CommandRequest::SetSessionMode {
            request_id: "mode-1".into(),
            mode_id: "plan".into(),
        })
        .await
        .unwrap();
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), observed_rx)
        .await
        .expect("Hel must send a mode request")
        .expect("the bridge must publish the request");
    drop(request_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), driver).await;
    bridge.abort();
    events.abort();
    observed
}

#[tokio::test]
async fn legacy_modes_use_session_set_mode() {
    let request = mode_change_request(ModeSurface::Legacy).await;

    assert_eq!(request["method"], "session/set_mode");
    assert_eq!(request["params"]["modeId"], "plan");
}

#[tokio::test]
async fn set_session_mode_uses_the_mode_protocol_even_when_config_is_available() {
    let request = mode_change_request(ModeSurface::Both).await;

    assert_eq!(request["method"], "session/set_mode");
}

#[tokio::test]
async fn unconstrained_policy_is_enforced_before_the_session_is_reported() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(mode_change_bridge(
        bridge_stream,
        ModeSurface::Both,
        observed_tx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });

    let request = tokio::time::timeout(std::time::Duration::from_secs(5), observed_rx)
        .await
        .expect("Hel must enforce the target execution policy")
        .expect("the bridge must publish the request");
    assert_eq!(request["method"], "session/set_config_option");
    assert_eq!(request["params"]["value"], "agent-full-access");

    let mut reported_mode = None;
    let mut configured_mode = None;
    while reported_mode.is_none() || configured_mode.is_none() {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .expect("the configured session must be reported")
            .expect("the runtime must keep its event channel open");
        match event {
            RuntimeEvent::SessionStarted { execution_mode, .. } => {
                reported_mode = execution_mode;
            }
            RuntimeEvent::SessionConfigured { config_options } => {
                configured_mode = Some(
                    serde_json::to_value(config_options).unwrap()[0]["currentValue"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    assert_eq!(reported_mode.as_deref(), Some("agent-full-access"));
    assert_eq!(configured_mode.as_deref(), Some("agent-full-access"));

    drop(request_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), driver).await;
    bridge.abort();
}

#[tokio::test]
async fn a_grok_effort_change_goes_out_as_a_legacy_set_model_request() {
    let request = config_change_request(HarnessKind::Grok, true, "effort", "low").await;

    assert_eq!(request["method"], "session/set_model");
    assert_eq!(request["params"]["sessionId"], "scripted");
    assert_eq!(request["params"]["modelId"], "grok-4.6");
    assert_eq!(request["params"]["_meta"]["reasoningEffort"], "low");
}

#[tokio::test]
async fn a_grok_model_change_goes_out_as_a_legacy_set_model_request() {
    let request = config_change_request(HarnessKind::Grok, true, "model", "grok-4.5").await;

    assert_eq!(request["method"], "session/set_model");
    assert_eq!(request["params"]["modelId"], "grok-4.5");
    assert!(
        request["params"].get("_meta").is_none(),
        "a model change carries no effort meta: {request}"
    );
}

#[tokio::test]
async fn a_grok_real_config_option_still_uses_the_standard_acp_request() {
    let request = config_change_request(HarnessKind::Grok, true, "verbosity", "detailed").await;

    assert_eq!(request["method"], "session/set_config_option");
    assert_eq!(request["params"]["configId"], "verbosity");
    assert_eq!(request["params"]["value"], "detailed");
}

#[tokio::test]
async fn a_harness_with_real_config_options_still_uses_the_standard_acp_request() {
    let request = config_change_request(HarnessKind::Claude, false, "model", "opus").await;

    assert_eq!(request["method"], "session/set_config_option");
    assert_eq!(request["params"]["configId"], "model");
    assert_eq!(request["params"]["value"], "opus");
}

#[tokio::test]
async fn a_failed_prompt_fails_the_turn_and_the_runtime_keeps_serving() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let bridge = tokio::spawn(scripted_bridge(bridge_stream));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());

    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });

    let next_event = async |events: &mut mpsc::Receiver<RuntimeEvent>| {
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the runtime must keep emitting events after a failed prompt")
            .expect("the runtime must not drop its event channel")
    };

    request_tx
        .send(CommandRequest::Prompt {
            request_id: "first".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
        })
        .await
        .unwrap();
    let mut warning = None;
    let failed = loop {
        match next_event(&mut event_rx).await {
            RuntimeEvent::Warning { message } => warning = Some(message),
            RuntimeEvent::PromptFinished {
                request_id,
                stop_reason,
            } => break (request_id, stop_reason),
            _ => {}
        }
    };
    assert_eq!(failed, ("first".to_owned(), "error".to_owned()));
    let warning = warning.expect("a failed prompt must warn before it finishes the turn");
    assert!(warning.contains("Authentication required"), "{warning}");
    assert!(crate::hel_credentials::auth_failure_signature(
        HarnessKind::Claude,
        &warning
    ));

    request_tx
        .send(CommandRequest::Prompt {
            request_id: "second".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("still there?"))],
        })
        .await
        .unwrap();
    let mut empty_warning = None;
    let completed = loop {
        match next_event(&mut event_rx).await {
            RuntimeEvent::Warning { message } => empty_warning = Some(message),
            RuntimeEvent::PromptFinished {
                request_id,
                stop_reason,
            } => break (request_id, stop_reason),
            _ => {}
        }
    };
    assert_eq!(completed, ("second".to_owned(), "EndTurn".to_owned()));
    assert_eq!(empty_warning.as_deref(), Some(PROMPT_EMPTY_RESPONSE_MARKER));

    request_tx
        .send(CommandRequest::Prompt {
            request_id: "third".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("answer this"))],
        })
        .await
        .unwrap();
    let mut saw_update = false;
    let mut warning = None;
    let completed = loop {
        match next_event(&mut event_rx).await {
            RuntimeEvent::SessionUpdate { .. } => saw_update = true,
            RuntimeEvent::Warning { message } => warning = Some(message),
            RuntimeEvent::PromptFinished {
                request_id,
                stop_reason,
            } => break (request_id, stop_reason),
            _ => {}
        }
    };
    assert_eq!(completed, ("third".to_owned(), "EndTurn".to_owned()));
    assert!(saw_update, "the scripted response must publish its update");
    assert_eq!(warning, None, "a response with output must not warn");

    drop(request_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), driver)
        .await
        .expect("closing the command channel must end the runtime")
        .expect("the runtime task must not panic")
        .expect("a failed prompt must not fail the runtime");
    assert_eq!(bridge.await.unwrap(), 3);
}

/// Answers `initialize` and `session/new`, then holds `session/prompt`
/// until the test completes it. Used to prove cancel waits for a real
/// prompt settlement and restarts when that settlement never arrives.
async fn stalled_prompt_bridge(
    stream: tokio::io::DuplexStream,
    observed: mpsc::UnboundedSender<String>,
    mut complete: mpsc::Receiver<()>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut prompt_id = None;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.expect("read stalled bridge input") else {
                    break;
                };
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
                let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let _ = observed.send(method.to_owned());
                let id = request
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let response = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"protocolVersion": 1},
                    }),
                    "session/new" | "session/load" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"sessionId": "scripted"},
                    }),
                    "session/prompt" => {
                        prompt_id = Some(id);
                        continue;
                    }
                    _ => continue,
                };
                if write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            complete = complete.recv() => {
                if complete.is_none() {
                    break;
                }
                let Some(id) = prompt_id.take() else {
                    continue;
                };
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"stopReason": "cancelled"},
                });
                if write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Read scripted-bridge methods into `methods` until a `session/prompt`
/// arrives, so a test can assert what the bridge saw and in what order.
async fn wait_for_bridge_prompt(
    observed: &mut mpsc::UnboundedReceiver<String>,
    methods: &mut Vec<String>,
) {
    loop {
        let method = tokio::time::timeout(Duration::from_secs(5), observed.recv())
            .await
            .expect("the prompt must reach the bridge")
            .expect("the bridge must keep reporting methods");
        let is_prompt = method == "session/prompt";
        methods.push(method);
        if is_prompt {
            return;
        }
    }
}

/// Answers `initialize`, `session/load`, and every `session/prompt` at once,
/// reporting each prompt's text so a test can tell which prompts reached it.
async fn prompt_echoing_bridge(
    stream: tokio::io::DuplexStream,
    observed: mpsc::UnboundedSender<String>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await.expect("read echoing bridge input") {
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
        let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match method {
            "initialize" => {
                let _ = observed.send(method.to_owned());
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}})
            }
            "session/new" | "session/load" => {
                let _ = observed.send(method.to_owned());
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "scripted"}})
            }
            "session/prompt" => {
                let text = request
                    .pointer("/params/prompt/0/text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let _ = observed.send(format!("session/prompt:{text}"));
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})
            }
            _ => continue,
        };
        if write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn wait_for_runtime_event<F>(
    events: &mut mpsc::Receiver<RuntimeEvent>,
    mut matches: F,
) -> RuntimeEvent
where
    F: FnMut(&RuntimeEvent) -> bool,
{
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("runtime event arrives")
            .expect("runtime event channel stays open");
        if matches(&event) {
            return event;
        }
    }
}

async fn steering_bridge(
    stream: tokio::io::DuplexStream,
    observed: mpsc::UnboundedSender<serde_json::Value>,
    mut complete: mpsc::Receiver<()>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut prompt_id = None;
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.expect("read steering bridge input") else {
                    break;
                };
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
                let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let _ = observed.send(request.clone());
                let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let response = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "_meta": {"steering": {"supported": true}},
                        },
                    }),
                    "session/new" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"sessionId": "steering-session"},
                    }),
                    "session/prompt" => {
                        prompt_id = Some(id);
                        continue;
                    }
                    SESSION_STEERING_METHOD => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"outcome": "injected"},
                    }),
                    _ => continue,
                };
                if write.write_all(format!("{response}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            complete = complete.recv() => {
                if complete.is_none() {
                    break;
                }
                let Some(id) = prompt_id.take() else {
                    continue;
                };
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"stopReason": "end_turn"},
                });
                if write.write_all(format!("{response}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[tokio::test]
async fn cancel_steers_the_queued_prompt_when_the_agent_supports_it() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (complete_tx, complete_rx) = mpsc::channel(1);
    let bridge = tokio::spawn(steering_bridge(bridge_stream, observed_tx, complete_rx));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });

    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-1".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("start"))],
        })
        .await
        .unwrap();
    loop {
        let request = observed_rx.recv().await.expect("prompt reaches bridge");
        if request["method"] == "session/prompt" {
            break;
        }
    }
    request_tx
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: Some(ClaimedSteeringPrompt {
                queued_command_id: "queued-1".into(),
                prompt: vec![ContentBlock::Text(TextContent::new("change direction"))],
            }),
        })
        .await
        .unwrap();

    let steering = loop {
        let request = observed_rx.recv().await.expect("steer reaches bridge");
        if request["method"] == SESSION_STEERING_METHOD {
            break request;
        }
        assert_ne!(request["method"], "session/cancel");
    };
    assert_eq!(steering["params"]["sessionId"], "steering-session");
    assert_eq!(steering["params"]["prompt"][0]["text"], "change direction");
    assert_eq!(
        steering["params"]["_meta"]["steering"]["idleBehavior"],
        "promptRequired"
    );
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::SteerApplied {
                request_id,
                queued_command_id,
            } if request_id == "cancel-1" && queued_command_id == "queued-1"
        )
    })
    .await;
    assert!(
        observed_rx.try_recv().is_err(),
        "steering must not send cancel"
    );

    complete_tx.send(()).await.unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::PromptFinished { request_id, .. } if request_id == "prompt-1"
        )
    })
    .await;
    drop(request_tx);
    tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("runtime exits")
        .expect("runtime task does not panic")
        .expect("steering does not fail the runtime");
    bridge.abort();
}

#[tokio::test(start_paused = true)]
async fn acknowledged_cancel_keeps_the_bridge_for_the_next_prompt() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (complete_tx, complete_rx) = mpsc::channel(1);
    let bridge = tokio::spawn(stalled_prompt_bridge(
        bridge_stream,
        observed_tx,
        complete_rx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    let mut methods = Vec::new();
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-1".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("go"))],
        })
        .await
        .unwrap();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;
    request_tx
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
            matches!(event, RuntimeEvent::CancelApplied { request_id } if request_id == "cancel-1")
        })
        .await;
    tokio::time::advance(CANCEL_ACK_TIMEOUT - Duration::from_secs(1)).await;
    complete_tx.send(()).await.unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::PromptFinished { request_id, .. } if request_id == "prompt-1"
        )
    })
    .await;

    // The acknowledged cancel leaves the bridge in place, so the next prompt
    // runs on the same connection and the same native session.
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-2".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("carry on"))],
        })
        .await
        .unwrap();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;
    complete_tx.send(()).await.unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::PromptFinished { request_id, .. } if request_id == "prompt-2"
        )
    })
    .await;
    drop(request_tx);
    let restart = tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("runtime exits")
        .expect("runtime task does not panic")
        .expect("a cancelled prompt must not fail the runtime");
    // `run_inner` emits `HarnessRestarting` exactly when the bridge asks for a
    // restart, so no restart request means no restart event.
    assert_eq!(
        restart, None,
        "an acknowledged cancel must not restart the bridge"
    );
    while let Ok(event) = event_rx.try_recv() {
        assert!(
            !matches!(event, RuntimeEvent::HarnessRestarting { .. }),
            "unexpected restart: {event:?}"
        );
    }
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "initialize")
            .count(),
        1,
        "the second prompt must not re-handshake: {methods:?}"
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "session/new" || *method == "session/load")
            .count(),
        1,
        "the second prompt must reuse the open session: {methods:?}"
    );
    bridge.abort();
}

#[tokio::test(start_paused = true)]
async fn unacked_cancel_restarts_the_harness_after_sixty_seconds() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (_complete_tx, complete_rx) = mpsc::channel(1);
    let bridge = tokio::spawn(stalled_prompt_bridge(
        bridge_stream,
        observed_tx,
        complete_rx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec,
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-1".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("go"))],
        })
        .await
        .unwrap();
    loop {
        let method = tokio::time::timeout(std::time::Duration::from_secs(5), observed_rx.recv())
            .await
            .expect("the prompt must reach the bridge")
            .expect("the bridge must keep reporting methods");
        if method == "session/prompt" {
            break;
        }
    }
    request_tx
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
            matches!(event, RuntimeEvent::CancelApplied { request_id } if request_id == "cancel-1")
        })
        .await;
    tokio::time::advance(CANCEL_ACK_TIMEOUT).await;
    let interrupted = wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::CommandInterrupted { request_id, .. } if request_id == "prompt-1"
        )
    })
    .await;
    let RuntimeEvent::CommandInterrupted { message, .. } = interrupted else {
        panic!("expected interrupt: {interrupted:?}");
    };
    assert!(message.contains("60s"), "{message}");
    drop(request_tx);
    let restart = tokio::time::timeout(std::time::Duration::from_secs(5), driver)
        .await
        .expect("runtime exits after an unacked cancel")
        .expect("runtime task does not panic")
        .expect("an unacked cancel restarts instead of failing the runtime");
    assert_eq!(restart.as_deref(), Some("scripted"));
    bridge.abort();
}

#[tokio::test(start_paused = true)]
async fn a_request_queued_across_a_restart_never_reaches_the_fresh_bridge() {
    fn scripted_spec(resume_session: Option<String>) -> LaunchSpec {
        LaunchSpec {
            command: "scripted".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session,
            harness: HarnessKind::Kimi,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::hel_acp::StepClock::default(),
        }
    }

    // The first bridge stalls its prompt, so an unacknowledged cancel restarts
    // the harness the way `run_inner` would.
    let (first_client, first_bridge) = tokio::io::duplex(64 * 1024);
    let (first_observed_tx, mut first_observed_rx) = mpsc::unbounded_channel();
    let (_complete_tx, complete_rx) = mpsc::channel(1);
    let first = tokio::spawn(stalled_prompt_bridge(
        first_bridge,
        first_observed_tx,
        complete_rx,
    ));
    let (first_read, first_write) = tokio::io::split(first_client);
    let first_transport = ByteStreams::new(first_write.compat_write(), first_read.compat());
    let (request_tx, request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let first_events = event_tx.clone();
    let first_driver = tokio::spawn(async move {
        let mut request_rx = request_rx;
        let result = drive(
            first_transport,
            scripted_spec(None),
            &mut request_rx,
            first_events,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await;
        (result, request_rx)
    });
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-1".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("go"))],
        })
        .await
        .unwrap();
    let mut first_methods = Vec::new();
    wait_for_bridge_prompt(&mut first_observed_rx, &mut first_methods).await;
    request_tx
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
            matches!(event, RuntimeEvent::CancelApplied { request_id } if request_id == "cancel-1")
        })
        .await;
    tokio::time::advance(CANCEL_ACK_TIMEOUT).await;
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::CommandInterrupted { request_id, .. } if request_id == "prompt-1"
        )
    })
    .await;
    let (restart, mut request_rx) = tokio::time::timeout(Duration::from_secs(5), first_driver)
        .await
        .expect("runtime exits after an unacked cancel")
        .expect("runtime task does not panic");
    let restart = restart.expect("an unacked cancel restarts instead of failing the runtime");
    assert_eq!(restart.as_deref(), Some("scripted"));
    first.abort();

    // The worker queued this before it saw `HarnessRestarting`, so it is
    // already in the set the worker interrupted. The fresh bridge must drop it
    // instead of running it untracked on the reloaded session.
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "late-prompt".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("stale"))],
        })
        .await
        .unwrap();

    let (second_client, second_bridge) = tokio::io::duplex(64 * 1024);
    let (second_observed_tx, mut second_observed_rx) = mpsc::unbounded_channel();
    let second = tokio::spawn(prompt_echoing_bridge(second_bridge, second_observed_tx));
    let (second_read, second_write) = tokio::io::split(second_client);
    let second_transport = ByteStreams::new(second_write.compat_write(), second_read.compat());
    let second_driver = tokio::spawn(async move {
        drive(
            second_transport,
            scripted_spec(Some("scripted".into())),
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            true,
        )
        .await
    });
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::SessionConfigured { .. })
    })
    .await;

    // A command dispatched after the worker sees the fresh session does run.
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "after-restart".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("fresh"))],
        })
        .await
        .unwrap();
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(
            event,
            RuntimeEvent::PromptFinished { request_id, .. } if request_id == "after-restart"
        )
    })
    .await;
    drop(request_tx);
    let restart = tokio::time::timeout(Duration::from_secs(5), second_driver)
        .await
        .expect("the second bridge exits when its requests end")
        .expect("runtime task does not panic")
        .expect("the second bridge must not fail the runtime");
    assert_eq!(restart, None);
    let mut second_methods = Vec::new();
    while let Ok(method) = second_observed_rx.try_recv() {
        second_methods.push(method);
    }
    assert_eq!(
        second_methods,
        vec![
            "initialize".to_owned(),
            "session/load".to_owned(),
            "session/prompt:fresh".to_owned(),
        ],
        "the queued prompt must never reach the fresh bridge"
    );
    second.abort();
}

/// Terminals run real children in real process groups, which only Unix has.
#[cfg(unix)]
mod terminals {
    use super::*;

    /// Every wait carries this bound, so a handler that stalls the dispatch
    /// loop or a child that deadlocks on a full pipe fails the test in
    /// seconds instead of hanging the suite.
    const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// Answers `initialize` and `session/new`, writes the requests a test
    /// scripts, and republishes every answer Hel sends back.
    async fn client_request_bridge(
        stream: tokio::io::DuplexStream,
        mut scripted: mpsc::UnboundedReceiver<serde_json::Value>,
        answers: mpsc::UnboundedSender<serde_json::Value>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (read, mut write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
        loop {
            let outgoing = tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line.expect("read bridge input") else {
                        break;
                    };
                    let message: serde_json::Value =
                        serde_json::from_str(&line).expect("bridge input must be JSON-RPC");
                    let Some(method) =
                        message.get("method").and_then(serde_json::Value::as_str)
                    else {
                        // No method: an answer to one of the scripted requests.
                        if answers.send(message).is_err() {
                            break;
                        }
                        continue;
                    };
                    let id = message
                        .get("id")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    match method {
                        "initialize" => serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1},
                        }),
                        "session/new" => serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "result": {"sessionId": "scripted"},
                        }),
                        _ => continue,
                    }
                }
                request = scripted.recv() => {
                    let Some(request) = request else {
                        break;
                    };
                    request
                }
            };
            if write
                .write_all(format!("{outgoing}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    }

    /// The agent side of a scripted connection. Answers are collected by
    /// id, so a test can keep one request in flight while it sends others.
    struct ScriptedAgent {
        scripted: mpsc::UnboundedSender<serde_json::Value>,
        answers: mpsc::UnboundedReceiver<serde_json::Value>,
        received: BTreeMap<String, serde_json::Value>,
        sent: usize,
    }

    impl ScriptedAgent {
        fn send(&mut self, method: &str, params: serde_json::Value) -> String {
            self.sent += 1;
            let id = format!("agent-{}", self.sent);
            self.scripted
                .send(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }))
                .expect("the scripted bridge must accept requests");
            id
        }

        async fn answer(&mut self, id: &str) -> serde_json::Value {
            loop {
                if let Some(answer) = self.received.remove(id) {
                    return answer;
                }
                let answer = tokio::time::timeout(ANSWER_TIMEOUT, self.answers.recv())
                        .await
                        .expect(
                            "Hel must answer every terminal request instead of leaving the agent waiting",
                        )
                        .expect("the bridge must keep publishing answers");
                let answer_id = answer
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .expect("an answer must carry the request id")
                    .to_owned();
                self.received.insert(answer_id, answer);
            }
        }

        async fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            let id = self.send(method, params);
            self.answer(&id).await
        }
    }

    struct ScriptedRuntime {
        agent: ScriptedAgent,
        observed: Arc<Mutex<Vec<RuntimeEvent>>>,
        requests: mpsc::Sender<CommandRequest>,
        driver: tokio::task::JoinHandle<Result<Option<String>>>,
        bridge: tokio::task::JoinHandle<()>,
        events: tokio::task::JoinHandle<()>,
    }

    impl ScriptedRuntime {
        /// Close the command channel and wait for the runtime to finish,
        /// which is also what tears the terminals down.
        async fn stop(self) {
            drop(self.requests);
            let restart = tokio::time::timeout(ANSWER_TIMEOUT, self.driver)
                .await
                .expect("closing the command channel must end the runtime")
                .expect("the runtime task must not panic")
                .expect("terminal work must not fail the runtime");
            assert_eq!(restart, None);
            self.bridge.abort();
            self.events.abort();
        }
    }

    fn start_scripted_runtime() -> ScriptedRuntime {
        let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
        let (scripted_tx, scripted_rx) = mpsc::unbounded_channel();
        let (answers_tx, answers_rx) = mpsc::unbounded_channel();
        let bridge = tokio::spawn(client_request_bridge(
            bridge_stream,
            scripted_rx,
            answers_tx,
        ));
        let (client_read, client_write) = tokio::io::split(client_stream);
        let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());

        let (request_tx, mut request_rx) = mpsc::channel(4);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        // Drain events so a full channel can never be mistaken for silence,
        // and keep them so a test can read what the runtime reported.
        let observed = Arc::new(Mutex::new(Vec::new()));
        let recorder = observed.clone();
        let events = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                recorder
                    .lock()
                    .expect("observed events lock poisoned")
                    .push(event);
            }
        });
        let spec = LaunchSpec {
            command: "scripted".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: None,
            harness: HarnessKind::Kimi,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::hel_acp::StepClock::default(),
        };
        let driver = tokio::spawn(async move {
            drive(
                transport,
                spec,
                &mut request_rx,
                event_tx,
                Arc::new(Mutex::new(None)),
                false,
            )
            .await
        });
        ScriptedRuntime {
            agent: ScriptedAgent {
                scripted: scripted_tx,
                answers: answers_rx,
                received: BTreeMap::new(),
                sent: 0,
            },
            observed,
            requests: request_tx,
            driver,
            bridge,
            events,
        }
    }

    fn terminal_params(terminal_id: &str) -> serde_json::Value {
        serde_json::json!({"sessionId": "scripted", "terminalId": terminal_id})
    }

    /// Every close report a terminal made. Waits for the first, then keeps
    /// watching: a second report would arrive right behind it.
    async fn terminal_close_reports(
        observed: &Arc<Mutex<Vec<RuntimeEvent>>>,
        terminal_id: &str,
    ) -> Vec<RuntimeEvent> {
        let reports = || {
            observed
                .lock()
                .expect("observed events lock poisoned")
                .iter()
                .filter(|event| {
                    matches!(event, RuntimeEvent::TerminalClosed { terminal_id: id, .. }
                            if id == terminal_id)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        for _ in 0..100 {
            if !reports().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        reports()
    }

    async fn create_terminal(agent: &mut ScriptedAgent, params: serde_json::Value) -> String {
        let created = agent.call("terminal/create", params).await;
        assert!(
            created.get("result").is_some(),
            "terminal/create must be answered with a result, not the catch-all's \
                 method-not-found error: {created}"
        );
        created["result"]["terminalId"]
            .as_str()
            .unwrap_or_else(|| panic!("terminal/create must return a terminal id: {created}"))
            .to_owned()
    }

    #[tokio::test]
    async fn terminal_create_output_wait_and_release_round_trip() {
        let mut runtime = start_scripted_runtime();
        let terminal_id = create_terminal(
            &mut runtime.agent,
            serde_json::json!({
                "sessionId": "scripted",
                "command": "/bin/sh",
                // `PATH` proves the daemon environment is inherited rather
                // than replaced by the agent's additions.
                "args": ["-c", "printf 'ran %s %s' \"$MJ_TERMINAL_TEST\" \"${PATH:+inherited}\""],
                "env": [{"name": "MJ_TERMINAL_TEST", "value": "overlaid"}],
            }),
        )
        .await;

        let exited = runtime
            .agent
            .call("terminal/wait_for_exit", terminal_params(&terminal_id))
            .await;
        assert_eq!(exited["result"]["exitCode"], 0, "{exited}");

        let output = runtime
            .agent
            .call("terminal/output", terminal_params(&terminal_id))
            .await;
        assert_eq!(output["result"]["output"], "ran overlaid inherited");
        assert_eq!(output["result"]["truncated"], false);
        assert_eq!(output["result"]["exitStatus"]["exitCode"], 0);

        let released = runtime
            .agent
            .call("terminal/release", terminal_params(&terminal_id))
            .await;
        assert!(released.get("result").is_some(), "{released}");

        // A released terminal is gone, and Hel says so rather than hanging.
        let stale = runtime
            .agent
            .call("terminal/output", terminal_params(&terminal_id))
            .await;
        assert_eq!(stale["error"]["code"], -32602, "{stale}");
        assert!(
            stale["error"]["data"]
                .as_str()
                .is_some_and(|data| data.contains(&terminal_id)),
            "the error must name the terminal: {stale}"
        );

        runtime.stop().await;
    }

    #[tokio::test]
    async fn terminal_output_keeps_the_last_bytes_when_a_child_exceeds_the_limit() {
        let mut runtime = start_scripted_runtime();
        // 512 KiB is far past the 64 KiB pipe buffer: a supervisor that did
        // not drain the pipes while the child ran would block it forever,
        // and the answer timeouts would report that as a failure.
        let script = "data=0123456789abcdef; \
                          while [ ${#data} -lt 524288 ]; do data=\"$data$data\"; done; \
                          printf '%s' \"$data\"; printf 'TAIL-MARKER'";
        let limit = 8 * 1024;
        let terminal_id = create_terminal(
            &mut runtime.agent,
            serde_json::json!({
                "sessionId": "scripted",
                "command": "/bin/sh",
                "args": ["-c", script],
                "outputByteLimit": limit,
            }),
        )
        .await;

        let exited = runtime
            .agent
            .call("terminal/wait_for_exit", terminal_params(&terminal_id))
            .await;
        assert_eq!(exited["result"]["exitCode"], 0, "{exited}");

        let output = runtime
            .agent
            .call("terminal/output", terminal_params(&terminal_id))
            .await;
        let text = output["result"]["output"]
            .as_str()
            .unwrap_or_else(|| panic!("terminal/output must serve text: {output}"));
        assert!(
            text.len() <= limit,
            "served {} bytes for a {limit} byte limit",
            text.len()
        );
        assert!(
            text.ends_with("TAIL-MARKER"),
            "the retained output must be the tail, ended with {:?}",
            &text[text.len().saturating_sub(32)..]
        );
        assert_eq!(output["result"]["truncated"], true, "{output}");

        runtime.stop().await;
    }

    #[tokio::test]
    async fn terminal_kill_reports_the_signal_and_keeps_output_readable() {
        let mut runtime = start_scripted_runtime();
        let terminal_id = create_terminal(
            &mut runtime.agent,
            serde_json::json!({
                "sessionId": "scripted",
                "command": "printf running; exec sleep 300",
                "args": [],
            }),
        )
        .await;

        // The wait stays outstanding while the terminal runs: an inline
        // wait would stall the dispatch loop and nothing below could be
        // answered.
        let waiting = runtime
            .agent
            .send("terminal/wait_for_exit", terminal_params(&terminal_id));
        let mut running = String::new();
        for _ in 0..100 {
            let polled = runtime
                .agent
                .call("terminal/output", terminal_params(&terminal_id))
                .await;
            running = polled["result"]["output"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if running == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(running, "running", "a live terminal must serve its output");

        let killed = runtime
            .agent
            .call("terminal/kill", terminal_params(&terminal_id))
            .await;
        assert!(killed.get("result").is_some(), "{killed}");

        let exited = runtime.agent.answer(&waiting).await;
        assert_eq!(exited["result"]["signal"], "SIGKILL", "{exited}");
        assert!(
            exited["result"].get("exitCode").is_none(),
            "a killed terminal has no exit code: {exited}"
        );

        // A kill does not release the terminal.
        let after = runtime
            .agent
            .call("terminal/output", terminal_params(&terminal_id))
            .await;
        assert_eq!(after["result"]["output"], "running");
        assert_eq!(after["result"]["exitStatus"]["signal"], "SIGKILL");

        let released = runtime
            .agent
            .call("terminal/release", terminal_params(&terminal_id))
            .await;
        assert!(released.get("result").is_some(), "{released}");

        // The transcript gets one report per terminal, from whichever of
        // kill, release, or teardown reaped the child.
        let observed = runtime.observed.clone();
        runtime.stop().await;
        let reports = terminal_close_reports(&observed, &terminal_id).await;
        assert_eq!(
            reports.len(),
            1,
            "a killed and released terminal must report its close once: {reports:?}"
        );
        let RuntimeEvent::TerminalClosed { output, signal, .. } = &reports[0] else {
            panic!("expected a terminal close report: {reports:?}");
        };
        assert_eq!(output, "running");
        assert_eq!(signal.as_deref(), Some("SIGKILL"));
    }

    #[tokio::test]
    async fn cancel_kills_live_client_terminals() {
        let mut runtime = start_scripted_runtime();
        let terminal_id = create_terminal(
            &mut runtime.agent,
            serde_json::json!({
                "sessionId": "scripted",
                "command": "printf running; exec sleep 300",
                "args": [],
            }),
        )
        .await;

        let waiting = runtime
            .agent
            .send("terminal/wait_for_exit", terminal_params(&terminal_id));
        let mut running = String::new();
        for _ in 0..100 {
            let polled = runtime
                .agent
                .call("terminal/output", terminal_params(&terminal_id))
                .await;
            running = polled["result"]["output"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if running == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(running, "running");

        runtime
            .requests
            .send(CommandRequest::Cancel {
                request_id: "cancel-terminals".into(),
                steering_prompt: None,
            })
            .await
            .unwrap();

        let exited = tokio::time::timeout(ANSWER_TIMEOUT, runtime.agent.answer(&waiting))
            .await
            .expect("cancel must kill the terminal so wait_for_exit can finish");
        assert_eq!(exited["result"]["signal"], "SIGKILL", "{exited}");

        runtime.stop().await;
    }

    #[tokio::test]
    async fn terminal_create_accepts_a_grok_style_single_string_command() {
        let mut runtime = start_scripted_runtime();
        // Grok Build puts the whole shell line in `command` and sends no
        // arguments at all.
        let terminal_id = create_terminal(
            &mut runtime.agent,
            serde_json::json!({
                "sessionId": "scripted",
                "command": "/bin/sh -c 'printf grok-ok'",
                "args": [],
            }),
        )
        .await;

        let exited = runtime
            .agent
            .call("terminal/wait_for_exit", terminal_params(&terminal_id))
            .await;
        assert_eq!(exited["result"]["exitCode"], 0, "{exited}");

        let output = runtime
            .agent
            .call("terminal/output", terminal_params(&terminal_id))
            .await;
        assert_eq!(output["result"]["output"], "grok-ok", "{output}");

        runtime.stop().await;
    }

    /// A process still visible but already dead — a zombie waiting for its
    /// parent — counts as gone; the parent died with it.
    fn process_is_gone(pid: i32) -> bool {
        // SAFETY: signal 0 only probes whether the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit(')')
                    .next()
                    .map(|rest| rest.trim_start().starts_with('Z'))
            })
            .unwrap_or(false)
    }

    /// A shell that keeps a grandchild alive and publishes both pids, so a
    /// test can prove a kill reached the whole process group rather than
    /// only the shell Hel spawned.
    async fn start_terminal_with_a_grandchild(
        runtime: &mut ScriptedRuntime,
        pids_path: &std::path::Path,
    ) -> Vec<i32> {
        let script = format!(
            "sleep 300 & printf '%s %s' \"$$\" \"$!\" > '{}'; wait",
            pids_path.display()
        );
        create_terminal(
            &mut runtime.agent,
            serde_json::json!({
                "sessionId": "scripted",
                "command": "/bin/sh",
                "args": ["-c", script],
            }),
        )
        .await;

        let mut pids = Vec::new();
        for _ in 0..250 {
            if let Ok(recorded) = std::fs::read_to_string(pids_path) {
                pids = recorded
                    .split_whitespace()
                    .filter_map(|pid| pid.parse::<i32>().ok())
                    .collect();
                if pids.len() == 2 {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(pids.len(), 2, "the terminal must report both of its pids");
        pids
    }

    async fn assert_processes_are_gone(pids: &[i32]) {
        for pid in pids {
            let mut gone = false;
            for _ in 0..250 {
                if process_is_gone(*pid) {
                    gone = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(gone, "process {pid} survived the runtime that started it");
        }
    }

    #[tokio::test]
    async fn runtime_teardown_kills_terminal_process_groups() {
        let temp = tempfile::tempdir().unwrap();
        let pids_path = temp.path().join("pids");
        let mut runtime = start_scripted_runtime();
        // Nothing killed or released this terminal: teardown owns it.
        let pids = start_terminal_with_a_grandchild(&mut runtime, &pids_path).await;

        runtime.stop().await;

        assert_processes_are_gone(&pids).await;
    }

    #[tokio::test]
    async fn dropping_the_connection_kills_terminal_process_groups() {
        let temp = tempfile::tempdir().unwrap();
        let pids_path = temp.path().join("pids");
        let mut runtime = start_scripted_runtime();
        let pids = start_terminal_with_a_grandchild(&mut runtime, &pids_path).await;

        // A bridge that dies mid-session leaves the runtime dropping the
        // whole connection rather than ending its command loop, so orderly
        // teardown never runs and the terminals still must not survive.
        runtime.driver.abort();

        assert_processes_are_gone(&pids).await;
        runtime.bridge.abort();
        runtime.events.abort();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dead_bridge_after_session_start_reloads_the_native_session() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("second-bridge");
    let script = temp.path().join("dying_acp.py");
    std::fs::write(
        &script,
        format!(
            r#"
import json, os, sys
marker = {marker:?}

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

second = os.path.exists(marker)
while True:
    request = read()
    if request is None:
        break
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        write({{"jsonrpc": "2.0", "id": ident, "result": {{"protocolVersion": 1}}}})
    elif method in ("session/new", "session/load"):
        servers = request.get("params", {{}}).get("mcpServers", [])
        if method == "session/new":
            assert len(servers) == 1, request
        else:
            assert not servers, request
            write({{"jsonrpc": "2.0", "method": "session/update", "params": {{
                "sessionId": "scripted",
                "update": {{
                    "sessionUpdate": "agent_message_chunk",
                    "content": {{"type": "text", "text": "replayed old history"}}
                }}
            }}}})
        write({{"jsonrpc": "2.0", "id": ident, "result": {{"sessionId": "scripted"}}}})
        if method == "session/load":
            # Codex can finish dispatching an old tool completion after the
            # load response. Its creation belongs to pre-resume history and
            # was intentionally not injected into this connection.
            write({{"jsonrpc": "2.0", "method": "session/update", "params": {{
                "sessionId": "scripted",
                "update": {{
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "old-wait-tool",
                    "status": "completed",
                    "title": "wait"
                }}
            }}}})
        if not second:
            open(marker, "w").close()
            import time
            time.sleep(0.2)
            break
    elif ident is not None:
        write({{"jsonrpc": "2.0", "id": ident, "result": {{}}}})
"#,
        ),
    )
    .unwrap();

    let (request_tx, request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: Some(ProjectMemoryLaunchConfig {
            project_key: "abc".into(),
            root: temp.path().join("memory"),
            baseline_root: temp.path().join("baseline"),
            repository_roots: BTreeMap::new(),
            mcp_delivery: ProjectMemoryMcpDelivery::Acp,
        }),
        resume_session: None,
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let runtime = tokio::spawn(run(spec, request_rx, event_tx));

    let mut started = Vec::new();
    let mut saw_reload = false;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .expect("ACP runtime keeps reporting")
            .expect("event channel stays open");
        match event {
            RuntimeEvent::SessionStarted {
                native_session_id,
                resumed,
                ..
            } => {
                assert_eq!(native_session_id, "scripted");
                started.push(resumed);
                if started.len() == 2 {
                    break;
                }
            }
            RuntimeEvent::HarnessRestarting { message } => {
                assert!(
                    message.contains("reloading the native session"),
                    "{message}"
                );
                saw_reload = true;
            }
            RuntimeEvent::SessionUpdate { update } => {
                panic!("resume replay leaked into the relay: {update}")
            }
            RuntimeEvent::Stopped => panic!("worker stopped before reloading the session"),
            _ => {}
        }
    }
    assert!(saw_reload, "a dead bridge after session start must reload");
    assert_eq!(started, vec![false, true], "the second open is a resume");

    drop(request_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), runtime)
        .await
        .expect("closing the command channel must end the runtime")
        .expect("runtime task does not panic")
        .expect("a recovered bridge must not fail the worker");
}

#[cfg(unix)]
#[tokio::test]
async fn an_acknowledged_cancel_keeps_the_running_bridge() {
    let temp = tempfile::tempdir().unwrap();
    let prompt_seen = temp.path().join("prompt-seen");
    let initializes = temp.path().join("initializes");
    let script = temp.path().join("cancelled_acp.py");
    std::fs::write(
        &script,
        format!(
            r#"
import json, sys
prompt_seen = {prompt_seen:?}
initializes = {initializes:?}

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

prompts = 0
while True:
    request = read()
    if request is None:
        break
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        with open(initializes, "a") as handle:
            handle.write("x")
        write({{"jsonrpc": "2.0", "id": ident, "result": {{"protocolVersion": 1}}}})
    elif method in ("session/new", "session/load"):
        write({{"jsonrpc": "2.0", "id": ident, "result": {{"sessionId": "scripted"}}}})
    elif method == "session/prompt":
        prompts += 1
        if prompts == 1:
            open(prompt_seen, "w").close()
            cancellation = read()
            assert cancellation.get("method") == "session/cancel", cancellation
            write({{"jsonrpc": "2.0", "id": ident, "result": {{"stopReason": "cancelled"}}}})
        else:
            write({{"jsonrpc": "2.0", "id": ident, "result": {{"stopReason": "end_turn"}}}})
    elif ident is not None:
        write({{"jsonrpc": "2.0", "id": ident, "result": {{}}}})
"#,
        ),
    )
    .unwrap();

    let (request_tx, request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };
    let runtime = tokio::spawn(run(spec, request_rx, event_tx));

    let mut finished = Vec::new();
    let wait_for_finished_prompt =
        async |event_rx: &mut mpsc::Receiver<RuntimeEvent>, finished: &mut Vec<String>| {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                    .await
                    .expect("the ACP runtime keeps reporting")
                    .expect("the event channel stays open");
                match event {
                    RuntimeEvent::HarnessRestarting { message } => {
                        panic!("an acknowledged cancel must not restart the bridge: {message}")
                    }
                    RuntimeEvent::PromptFinished { request_id, .. } => {
                        finished.push(request_id);
                        return;
                    }
                    _ => {}
                }
            }
        };

    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::SessionStarted { resumed: false, .. })
    })
    .await;
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-1".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("go"))],
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !prompt_seen.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the prompt reaches the bridge");
    request_tx
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    wait_for_finished_prompt(&mut event_rx, &mut finished).await;

    // The bridge that acknowledged the cancel serves the next prompt too.
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-2".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("carry on"))],
        })
        .await
        .unwrap();
    wait_for_finished_prompt(&mut event_rx, &mut finished).await;
    assert_eq!(finished, vec!["prompt-1".to_owned(), "prompt-2".to_owned()]);
    assert_eq!(
        std::fs::read_to_string(&initializes).unwrap(),
        "x",
        "the second prompt must run on the bridge that was already open"
    );

    drop(request_tx);
    tokio::time::timeout(Duration::from_secs(5), runtime)
        .await
        .expect("closing the command channel ends the runtime")
        .expect("runtime task does not panic")
        .expect("an acknowledged cancel keeps the runtime healthy");
}

#[cfg(unix)]
#[tokio::test]
async fn bridge_exit_during_initialize_returns_an_actionable_error() {
    let (_request_tx, request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let spec = LaunchSpec {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "echo 'specific supervisor failure' >&2; exit 17".into(),
        ],
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        run(spec, request_rx, event_tx),
    )
    .await
    .expect("an exited bridge must not leave ACP initialization hanging")
    .unwrap_err();
    let complete_error = format!("{error:#}");
    assert!(
        complete_error.contains("bridge stdout must contain only JSON-RPC frames"),
        "unexpected error: {error:#}"
    );
    assert!(complete_error.contains("specific supervisor failure"));

    let events = std::iter::from_fn(|| event_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Warning { message } if
            message.contains("ACP runtime failed")))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Stopped))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bridge_launch_failure_is_reported_before_the_runtime_stops() {
    let temp = tempfile::tempdir().unwrap();
    let (_request_tx, request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let missing_bridge = temp.path().join("missing-acp-bridge");
    let spec = LaunchSpec {
        command: missing_bridge.clone(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::hel_acp::StepClock::default(),
    };

    let error = run(spec, request_rx, event_tx).await.unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!("launch ACP bridge {}", missing_bridge.display()))
    );
    assert!(matches!(
        event_rx.recv().await,
        Some(RuntimeEvent::Warning { message }) if message.contains("ACP runtime failed")
    ));
    assert!(matches!(event_rx.recv().await, Some(RuntimeEvent::Stopped)));
}

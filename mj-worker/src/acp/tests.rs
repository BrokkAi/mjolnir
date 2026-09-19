use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use super::*;
use agent_client_protocol::schema::v1::{
    SessionConfigSelectGroup, SessionConfigSelectOption, SessionConfigSelectOptions, ToolCallUpdate,
};

/// Every launch request states the servers Mjolnir owns. A bridge that opens
/// the session again is a new harness process, and Codex builds the resumed
/// thread's MCP set from the request alone (#1085), so a resume that sends
/// none leaves the session without its delegation tools.
#[test]
fn every_launch_request_states_the_mjolnir_owned_mcp_servers() {
    let mut spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "codex-acp".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };
    let request = serde_json::to_value(new_session_request(&spec, true)).unwrap();
    assert_eq!(
        request.get("mcpServers"),
        Some(&serde_json::json!([])),
        "a new session always states its MCP set, even when it is empty"
    );
    for opened in [
        serde_json::to_value(resume_session_request(&spec, SessionId::from("native"))).unwrap(),
        serde_json::to_value(load_session_request(&spec, SessionId::from("native"))).unwrap(),
    ] {
        assert!(
            opened
                .get("mcpServers")
                .is_none_or(|servers| servers == &serde_json::json!([])),
            "a session with no Mjolnir servers asks for none: {opened}"
        );
    }

    spec.subagent_mcp_socket = Some("/worker/subagents.sock".into());
    let names = |request: &serde_json::Value| -> Vec<String> {
        request
            .get("mcpServers")
            .and_then(serde_json::Value::as_array)
            .expect("every launch request states its MCP set")
            .iter()
            .map(|server| server["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    for request in [
        serde_json::to_value(new_session_request(&spec, true)).unwrap(),
        serde_json::to_value(resume_session_request(&spec, SessionId::from("native"))).unwrap(),
        serde_json::to_value(load_session_request(&spec, SessionId::from("native"))).unwrap(),
    ] {
        assert_eq!(
            names(&request),
            vec!["mj-agents".to_owned()],
            "the delegation server must survive a relaunch: {request}"
        );
    }
}

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

/// Native delegation is hidden only when Mjolnir's own delegation socket
/// replaced it. Without the socket the session keeps `Agent` and
/// `spawn_agent`, rather than ending up with neither.
#[test]
fn native_delegation_tools_are_hidden_only_when_the_subagent_socket_exists() {
    let mut spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "/worker/mj".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };

    for harness in [HarnessKind::Claude, HarnessKind::Codex] {
        spec.harness = harness;

        spec.subagent_mcp_socket = None;
        let meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
        assert!(
            meta.pointer("/claudeCode/options/disallowedTools")
                .is_none(),
            "{harness:?} without a socket must keep its native tools: {meta}"
        );
        assert!(
            meta.pointer("/codex/options/disallowedTools").is_none(),
            "{harness:?} without a socket must keep its native tools: {meta}"
        );

        spec.subagent_mcp_socket = Some("/worker/subagents.sock".into());
        let meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
        let hidden = match harness {
            HarnessKind::Claude => meta.pointer("/claudeCode/options/disallowedTools"),
            _ => meta.pointer("/codex/options/disallowedTools"),
        };
        let expected = match harness {
            HarnessKind::Claude => {
                serde_json::json!(["Agent", "Task", "TaskOutput", "TaskStop"])
            }
            _ => serde_json::json!(["spawn_agent"]),
        };
        assert_eq!(hidden, Some(&expected), "{harness:?}: {meta}");
    }
}

#[test]
fn project_memory_mcp_honors_harness_delivery_and_claude_native_memory() {
    let mut spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "/worker/hel".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: vec!["/workspace/api".into()],
        extra_mcp_servers: Vec::new(),
        project_memory: Some(ProjectMemoryLaunchConfig {
            history_socket: None,
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
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };
    let servers = project_memory_mcp(&spec);
    let [McpServer::Stdio(server)] = servers.as_slice() else {
        panic!("non-Claude sessions receive exactly one memory MCP server");
    };
    assert_eq!(server.name, "mj-memory");
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
    claude.project_memory.as_mut().unwrap().history_socket = Some("/worker/control.sock".into());
    let servers = project_memory_mcp(&claude);
    let [McpServer::Stdio(server)] = servers.as_slice() else {
        panic!("Claude receives history tools");
    };
    assert!(server.args.contains(&"--native-notes".into()));
    assert!(server.args.contains(&"/worker/control.sock".into()));
    claude.harness = HarnessKind::Codex;
    let servers = project_memory_mcp(&claude);
    let [McpServer::Stdio(server)] = servers.as_slice() else {
        panic!("Codex receives history and notes");
    };
    assert!(!server.args.contains(&"--native-notes".into()));
    claude.harness = HarnessKind::Muse;
    assert!(project_memory_mcp(&claude).is_empty());
}

#[test]
fn claude_session_metadata_subscribes_to_background_task_levels_for_all_policies() {
    let mut spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "claude-agent-acp".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: vec!["/workspace/api".into()],
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };
    let meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
    assert_eq!(
        meta.pointer("/claudeCode/options/sandbox/enabled"),
        Some(&serde_json::Value::Bool(false))
    );
    assert_eq!(
        meta.pointer("/claudeCode/options/perTaskStopAffordance"),
        Some(&serde_json::Value::Bool(true))
    );
    let filter = serde_json::json!([{
        "type": "system",
        "subtype": "background_tasks_changed",
    }]);
    assert_eq!(
        meta.pointer("/claudeCode/emitRawSDKMessages"),
        Some(&filter)
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
            request.pointer("/_meta/claudeCode/options/perTaskStopAffordance"),
            Some(&serde_json::Value::Bool(true)),
            "{request}"
        );
        assert_eq!(
            request.pointer("/_meta/claudeCode/emitRawSDKMessages"),
            Some(&filter),
            "{request}"
        );
        assert_eq!(
            request["additionalDirectories"],
            serde_json::json!(["/workspace/api"]),
            "{request}"
        );
    }

    spec.execution_policy = ExecutionPolicy::ConfiguredApprovals;
    let configured_meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
    assert_eq!(
        configured_meta.pointer("/claudeCode/emitRawSDKMessages"),
        Some(&filter)
    );
    assert_eq!(
        configured_meta.pointer("/claudeCode/options/perTaskStopAffordance"),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(
        configured_meta
            .pointer("/claudeCode/options/sandbox")
            .is_none()
    );
    for request in [
        serde_json::to_value(new_session_request(&spec, true)).unwrap(),
        serde_json::to_value(load_session_request(&spec, SessionId::from("native"))).unwrap(),
    ] {
        assert_eq!(
            request.pointer("/_meta/claudeCode/emitRawSDKMessages"),
            Some(&filter),
            "{request}"
        );
        assert!(
            request
                .pointer("/_meta/claudeCode/options/sandbox")
                .is_none(),
            "{request}"
        );
    }
    spec.execution_policy = ExecutionPolicy::Unconstrained;
    spec.subagent_mcp_socket = Some("/worker/subagents.sock".into());
    let claude_meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
    assert_eq!(
        claude_meta.pointer("/claudeCode/options/disallowedTools"),
        Some(&serde_json::json!([
            "Agent",
            "Task",
            "TaskOutput",
            "TaskStop"
        ]))
    );
    assert!(
        extra_mcp(&spec).is_empty(),
        "Claude reads its staged MCP profile"
    );

    spec.harness = HarnessKind::Codex;
    let meta = serde_json::Value::Object(session_request_meta(&spec).unwrap());
    assert!(meta.get("claudeCode").is_none());
    assert_eq!(
        meta.pointer("/goal/resumePolicy"),
        Some(&serde_json::json!("preserve"))
    );
    assert_eq!(
        meta.pointer("/codex/options/disallowedTools"),
        Some(&serde_json::json!(["spawn_agent"]))
    );
    let servers = extra_mcp(&spec);
    let [McpServer::Stdio(server)] = servers.as_slice() else {
        panic!("Codex receives the Mjolnir sub-agent MCP server");
    };
    assert_eq!(server.name, "mj-agents");
    // The harness travels with the server because its own MCP client decides
    // how long one `wait` call may stay open.
    assert_eq!(
        server.args,
        [
            "worker",
            "subagent-mcp",
            "--socket",
            "/worker/subagents.sock",
            "--harness",
            "codex"
        ]
    );
}

#[test]
fn claude_async_task_updates_publish_only_stop_capability_changes() {
    assert_eq!(
        claude_async_task_control_update(&serde_json::json!({
            "sessionUpdate": "async_task_spawned",
            "asyncTaskId": "task-7",
            "canStop": true,
        }))
        .unwrap(),
        Some(ClaudeAsyncTaskControlUpdate::Set {
            task_id: "task-7".into(),
            can_stop: true,
        })
    );
    assert_eq!(
        claude_async_task_control_update(&serde_json::json!({
            "sessionUpdate": "async_task_state_update",
            "asyncTaskId": "task-7",
            "state": "stopped",
        }))
        .unwrap(),
        Some(ClaudeAsyncTaskControlUpdate::Set {
            task_id: "task-7".into(),
            can_stop: false,
        })
    );
    assert_eq!(
        claude_async_task_control_update(&serde_json::json!({
            "sessionUpdate": "async_task_progress",
            "asyncTaskId": "task-7",
        }))
        .unwrap(),
        Some(ClaudeAsyncTaskControlUpdate::Ignore)
    );
    assert!(
        claude_async_task_control_update(&serde_json::json!({
            "sessionUpdate": "async_task_spawned",
            "asyncTaskId": "",
            "canStop": true,
        }))
        .is_err()
    );
    assert_eq!(
        claude_async_task_control_update(&serde_json::json!({
            "sessionUpdate": "tool_call",
        }))
        .unwrap(),
        None
    );
}

#[test]
fn claude_async_task_stop_request_uses_the_air_wire_shape() {
    let request = ClaudeAsyncTaskStopRequest {
        session_id: SessionId::from("native-session"),
        async_task_id: "task-7".into(),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::json!({
            "sessionId": "native-session",
            "asyncTaskId": "task-7",
        })
    );
}

#[test]
fn resumed_session_request_keeps_load_context() {
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "claude-agent-acp".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: "/workspace/app".into(),
        additional_directories: vec!["/workspace/api".into()],
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: Some("native".into()),
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };
    let load = serde_json::to_value(load_session_request(&spec, SessionId::from("native")))
        .expect("load request serializes");
    let resume = serde_json::to_value(resume_session_request(&spec, SessionId::from("native")))
        .expect("resume request serializes");

    for field in ["sessionId", "cwd", "additionalDirectories", "_meta"] {
        assert_eq!(resume[field], load[field], "resume request changed {field}");
    }
}

#[test]
fn claude_sdk_messages_keep_only_non_ambient_background_task_levels() {
    let notification = <ClaudeSdkMessageNotification as agent_client_protocol::JsonRpcMessage>::parse_message(
        "_claude/sdkMessage",
        &serde_json::json!({
            "sessionId": "native",
            "message": {
                "type": "system",
                "subtype": "background_tasks_changed",
                "tasks": [
                    {"task_id": "server", "task_type": "shell", "description": "npm run dev"},
                    {"task_id": "watcher", "task_type": "watch", "description": "Watch files", "ambient": true},
                ],
            },
        }),
    )
    .expect("Claude extension notification deserializes");
    assert_eq!(notification.session_id.to_string(), "native");
    assert_eq!(
        claude_background_tasks(&notification.message).unwrap(),
        Some(vec![ClaudeBackgroundTask {
            task_id: "server".into(),
            description: "npm run dev".into(),
        }])
    );
    assert_eq!(
        claude_background_tasks(&serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "foreground",
        }))
        .unwrap(),
        None,
        "edge lifecycle messages must not become background levels"
    );
}

#[tokio::test]
async fn claude_sdk_extension_notification_reaches_runtime_without_opening_a_step() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    for (resume_session, advertised_resume) in [
        (None, false),
        (None, true),
        (Some("native"), false),
        (Some("native"), true),
    ] {
        let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
        let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
        let bridge = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(bridge_stream);
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.expect("read fake adapter input") {
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("fake adapter input is JSON-RPC");
                let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let _ = observed_tx.send(request.clone());
                let id = request
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let response = match method {
                    "initialize" => {
                        let mut result = serde_json::json!({"protocolVersion": 1});
                        if advertised_resume {
                            result["agentCapabilities"] = serde_json::json!({
                                "sessionCapabilities": {"resume": {}}
                            });
                        }
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result,
                        })
                    }
                    "session/new" | "session/load" | "session/resume" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"sessionId": "scripted", "modes": {
                            "currentModeId": "default",
                            "availableModes": [
                                {"id": "default", "name": "Default"},
                                {"id": "auto", "name": "Auto"}
                            ]
                        }},
                    }),
                    "session/set_mode" => {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
                    }
                    _ => continue,
                };
                if matches!(method, "session/new" | "session/load" | "session/resume") {
                    for message in [
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "_claude/sdkMessage",
                            "params": {
                                "sessionId": "scripted",
                                "message": {
                                    "type": "system",
                                    "subtype": "task_started",
                                    "task_id": "foreground",
                                },
                            },
                        }),
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "_claude/sdkMessage",
                            "params": {
                                "sessionId": "scripted",
                                "message": {
                                    "type": "system",
                                    "subtype": CLAUDE_BACKGROUND_TASKS_CHANGED_SUBTYPE,
                                    "tasks": [{
                                        "task_id": "server",
                                        "task_type": "shell",
                                        "description": "npm run dev",
                                    }],
                                },
                            },
                        }),
                    ] {
                        write
                            .write_all(format!("{message}\n").as_bytes())
                            .await
                            .expect("write fake adapter extension notification");
                    }
                }
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("write fake adapter response");
            }
        });
        let (client_read, client_write) = tokio::io::split(client_stream);
        let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let step_clock = crate::acp::StepClock::default();
        let observed_step_clock = step_clock.clone();
        let spec = LaunchSpec {
            bridge_spec_path: None,
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "scripted".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: resume_session.map(str::to_owned),
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness: HarnessKind::Claude,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock,
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

        let event = wait_for_runtime_event(&mut event_rx, |event| {
            matches!(event, RuntimeEvent::ClaudeBackgroundTasksChanged { .. })
        })
        .await;
        let RuntimeEvent::ClaudeBackgroundTasksChanged { tasks } = event else {
            unreachable!("wait_for_runtime_event matched the Claude level event");
        };
        assert_eq!(
            tasks,
            vec![ClaudeBackgroundTask {
                task_id: "server".into(),
                description: "npm run dev".into(),
            }]
        );
        assert_eq!(
            observed_step_clock.started_at_ms(),
            None,
            "a provider background level must not advance the step clock"
        );

        drop(request_tx);
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("closing the command channel ends the runtime")
            .expect("the runtime task does not panic")
            .expect("the fake adapter session ends cleanly");
        let mut session_requests = Vec::new();
        while let Ok(request) = observed_rx.try_recv() {
            if matches!(
                request["method"].as_str(),
                Some("session/new" | "session/load" | "session/resume")
            ) {
                session_requests.push(request);
            }
        }
        assert_eq!(session_requests.len(), 1, "open exactly one native session");
        assert_eq!(
            session_requests[0]["method"],
            if resume_session.is_none() {
                "session/new"
            } else if advertised_resume {
                "session/resume"
            } else {
                "session/load"
            },
            "session setup must select the advertised lifecycle method"
        );
        if resume_session.is_some() {
            assert_eq!(session_requests[0]["params"]["sessionId"], "native");
        }
        bridge.abort();
    }
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
fn muse_empty_model_selection_treats_first_advertised_choice_as_default() {
    let options = vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "",
            vec![
                SessionConfigSelectOption::new("muse-spark-1.3", "Muse Spark 1.3"),
                SessionConfigSelectOption::new("muse-spark-1.2", "Muse Spark 1.2"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
    ];

    assert!(muse_implicit_default(&options, "muse-spark-1.3"));
    assert!(!muse_implicit_default(&options, "muse-spark-1.2"));
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

#[test]
fn unconstrained_muse_permission_prefers_a_one_time_allow_and_never_cancels() {
    use agent_client_protocol::schema::v1::{
        PermissionOption, ToolCallUpdate, ToolCallUpdateFields,
    };

    let request = RequestPermissionRequest::new(
        "session-1",
        ToolCallUpdate::new("tool-1", ToolCallUpdateFields::new().title("Run command")),
        vec![
            PermissionOption::new("abort", "Abort", PermissionOptionKind::RejectOnce),
            PermissionOption::new("always", "Always allow", PermissionOptionKind::AllowAlways),
            PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        ],
    );

    let response = muse_unconstrained_permission_response(&request).unwrap();
    assert_eq!(
        serde_json::to_value(response).unwrap()["outcome"]["optionId"],
        "allow_once"
    );

    let rejected_only = RequestPermissionRequest::new(
        "session-1",
        ToolCallUpdate::new("tool-2", ToolCallUpdateFields::default()),
        vec![PermissionOption::new(
            "abort",
            "Abort",
            PermissionOptionKind::RejectOnce,
        )],
    );
    assert!(muse_unconstrained_permission_response(&rejected_only).is_none());
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
    assert!(mj_core::credentials::auth_failure_signature(
        HarnessKind::Claude,
        &auth
    ));

    let other = prompt_failure_warning(&agent_client_protocol::Error::internal_error());
    assert!(other.contains("prompt failed"), "{other}");
    assert!(!mj_core::credentials::auth_failure_signature(
        HarnessKind::Claude,
        &other
    ));
}

#[test]
fn only_a_finished_turn_that_produced_nothing_counts_as_unanswered() {
    assert!(prompt_returned_without_updates(&StopReason::EndTurn, 7, 7));
    assert!(!prompt_returned_without_updates(&StopReason::EndTurn, 7, 8));
    assert!(!prompt_returned_without_updates(
        &StopReason::Cancelled,
        7,
        7
    ));
    // A turn that ended for a reason of its own already reports that reason.
    // Relabelling it "unanswered" would hide why it really ended.
    for stop_reason in [
        StopReason::MaxTokens,
        StopReason::MaxTurnRequests,
        StopReason::Refusal,
    ] {
        assert!(
            !prompt_returned_without_updates(&stop_reason, 7, 7),
            "{stop_reason:?} must keep its own stop reason"
        );
    }
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
            "session/new" => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"sessionId": "scripted", "modes": {
                    "currentModeId": "default",
                    "availableModes": [{"id": "default", "name": "Default"}, {"id": "auto", "name": "Auto"}]
                }}
            }),
            "session/set_mode" => {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
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
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn", "usage": {"totalTokens": 30, "inputTokens": 20, "outputTokens": 10}}})
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Grok,
        execution_policy,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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
                "result": {"sessionId": "scripted", "modes": {
                    "currentModeId": "default",
                    "availableModes": [{"id": "default", "name": "Default"}, {"id": "auto", "name": "Auto"}]
                }},
            }),
            "session/set_mode" => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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
                    mj_core::elicitation::ElicitationValue::String("thin".into()),
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
                        // Claude's guardian policy selects Auto at startup.
                        "modes": {"currentModeId": "default", "availableModes": [
                            {"id": "default", "name": "Default"},
                            {"id": "auto", "name": "Auto"}
                        ]},
                    },
                })
            }
            // The startup mode enforcement is not the change under test.
            "session/set_mode" => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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

/// `ignored_mode` is the startup enforcement selection a test treats as setup
/// rather than as the mode change it is observing.
async fn mode_change_bridge(
    stream: tokio::io::DuplexStream,
    surface: ModeSurface,
    ignored_mode: Option<&str>,
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
                {"value": "auto", "name": "Auto"},
                {"value": "bypassPermissions", "name": "Bypass"},
                {"value": "agent-full-access", "name": "Full access"}
            ]
        })
    };
    let modes = serde_json::json!({
        // The target policy must be selected before exposing the session.
        "currentModeId": "agent",
            "availableModes": [
                {"id": "default", "name": "Default"},
                {"id": "plan", "name": "Plan"},
                {"id": "agent", "name": "Agent"},
                {"id": "auto", "name": "Auto"},
                {"id": "bypassPermissions", "name": "Bypass"},
                {"id": "agent-full-access", "name": "Full access"}
        ]
    });
    let is_ignored = |message: &serde_json::Value| {
        ignored_mode.is_some_and(|mode| {
            ["value", "modeId"]
                .iter()
                .any(|key| message["params"][key] == mode)
        })
    };
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
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}
            }),
            "session/new" | "session/load" => {
                let mut result = serde_json::json!({"sessionId": "scripted"});
                if matches!(surface, ModeSurface::Both) {
                    result["configOptions"] = serde_json::json!([mode_option("agent")]);
                }
                if matches!(surface, ModeSurface::Legacy | ModeSurface::Both) {
                    result["modes"] = modes.clone();
                }
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            "session/set_config_option" => {
                if !is_ignored(&message)
                    && let Some(observed) = observed.take()
                {
                    let _ = observed.send(message.clone());
                }
                let selected = message["params"]["value"].as_str().unwrap_or("default");
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"configOptions": [mode_option(selected)]}
                })
            }
            _ => {
                if !is_ignored(&message)
                    && let Some(observed) = observed.take()
                {
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
    // Claude selects Auto at startup; the change under test is the plan mode.
    let bridge = tokio::spawn(mode_change_bridge(
        bridge_stream,
        surface,
        Some("auto"),
        observed_tx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let events = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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

async fn policy_is_enforced_before_session_is_reported(
    harness: HarnessKind,
    execution_policy: ExecutionPolicy,
    resume_session: Option<&str>,
) {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(mode_change_bridge(
        bridge_stream,
        ModeSurface::Both,
        None,
        observed_tx,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: resume_session.map(str::to_owned),
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness,
        execution_policy,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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

    let (expected_mode, expected_label) = match (harness, execution_policy) {
        (HarnessKind::Codex, ExecutionPolicy::ConfiguredApprovals) => ("agent", "agent / guardian"),
        (HarnessKind::Codex, ExecutionPolicy::Unconstrained) => {
            ("agent-full-access", "agent-full-access")
        }
        (HarnessKind::Claude, ExecutionPolicy::ConfiguredApprovals) => ("auto", "auto / guardian"),
        (HarnessKind::Claude, ExecutionPolicy::Unconstrained) => {
            ("bypassPermissions", "bypassPermissions / sandbox-off")
        }
        (harness, policy) => panic!("unscripted harness {harness:?} under {policy:?}"),
    };
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), observed_rx)
        .await
        .expect("Hel must enforce the target execution policy")
        .expect("the bridge must publish the request");
    assert_eq!(request["method"], "session/set_config_option");
    assert_eq!(request["params"]["value"], expected_mode);

    let mut reported_mode = None;
    let mut reported_resumed = None;
    let mut configured_mode = None;
    while reported_mode.is_none() || configured_mode.is_none() {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .expect("the configured session must be reported")
            .expect("the runtime must keep its event channel open");
        match event {
            RuntimeEvent::SessionStarted {
                execution_mode,
                resumed,
                ..
            } => {
                reported_mode = execution_mode;
                reported_resumed = Some(resumed);
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
    assert_eq!(reported_mode.as_deref(), Some(expected_label));
    assert_eq!(reported_resumed, Some(resume_session.is_some()));
    assert_eq!(configured_mode.as_deref(), Some(expected_mode));

    drop(request_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), driver).await;
    bridge.abort();
}

#[tokio::test]
async fn codex_selects_guardian_for_a_new_session() {
    policy_is_enforced_before_session_is_reported(
        HarnessKind::Codex,
        ExecutionPolicy::ConfiguredApprovals,
        None,
    )
    .await;
}

#[tokio::test]
async fn codex_selects_target_policy_when_loading_a_session() {
    policy_is_enforced_before_session_is_reported(
        HarnessKind::Codex,
        ExecutionPolicy::ConfiguredApprovals,
        Some("native-session"),
    )
    .await;
}

#[tokio::test]
async fn unconstrained_policy_is_enforced_before_the_session_is_reported() {
    policy_is_enforced_before_session_is_reported(
        HarnessKind::Codex,
        ExecutionPolicy::Unconstrained,
        None,
    )
    .await;
}

/// Claude's guardian policy is its Auto mode, which it must select before the
/// session is reported; unconstrained sessions take bypassPermissions.
#[tokio::test]
async fn claude_selects_auto_for_guardian_and_bypass_when_unconstrained() {
    for policy in [
        ExecutionPolicy::ConfiguredApprovals,
        ExecutionPolicy::Unconstrained,
    ] {
        policy_is_enforced_before_session_is_reported(HarnessKind::Claude, policy, None).await;
    }
}

/// A harness that answers the mode request while still reporting another mode
/// has not applied the policy, so the session must fail instead of running
/// prompts under the permissions the operator did not ask for.
async fn stubborn_mode_bridge(stream: tokio::io::DuplexStream) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mode_option = serde_json::json!({
        "id": "interaction_mode",
        "name": "Mode",
        "category": "mode",
        "type": "select",
        // Never moves, whatever is selected.
        "currentValue": "default",
        "options": [
            {"value": "default", "name": "Default"},
            {"value": "bypassPermissions", "name": "Bypass"}
        ]
    });
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await.expect("read bridge input") {
        let message: serde_json::Value = serde_json::from_str(&line).unwrap();
        let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let id = message.get("id").cloned().unwrap_or_default();
        let result = match method {
            "initialize" => serde_json::json!({"protocolVersion": 1}),
            "session/new" => serde_json::json!({
                "sessionId": "scripted",
                "configOptions": [mode_option.clone()]
            }),
            "session/set_config_option" => {
                serde_json::json!({"configOptions": [mode_option.clone()]})
            }
            _ => serde_json::json!({}),
        };
        let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
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
async fn a_mode_the_harness_acknowledges_but_does_not_apply_fails_the_session() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let bridge = tokio::spawn(stubborn_mode_bridge(bridge_stream));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), driver)
        .await
        .expect("the session must stop instead of waiting for prompts")
        .expect("the driver task must not panic")
        .expect_err("an unapplied execution mode must fail the session");
    let message = format!("{error:#}");
    assert!(
        message.contains("acknowledged execution mode bypassPermissions")
            && message.contains("default"),
        "the failure must name the requested and the reported mode: {message}"
    );
    while let Ok(event) = event_rx.try_recv() {
        assert!(
            !matches!(event, RuntimeEvent::SessionStarted { .. }),
            "no session may be reported once the mode was refused"
        );
    }
    drop(request_tx);
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Claude,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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
                ..
            } => break (request_id, stop_reason),
            _ => {}
        }
    };
    assert_eq!(failed, ("first".to_owned(), "error".to_owned()));
    let warning = warning.expect("a failed prompt must warn before it finishes the turn");
    assert!(warning.contains("Authentication required"), "{warning}");
    assert!(mj_core::credentials::auth_failure_signature(
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
                usage,
                ..
            } => break (request_id, stop_reason, usage),
            _ => {}
        }
    };
    // The bridge called this turn a success and produced nothing, so it is
    // reported under its own stop reason rather than as a finished turn (#970).
    assert_eq!(
        (&completed.0, &completed.1),
        (
            &"second".to_owned(),
            &PROMPT_UNANSWERED_STOP_REASON.to_owned()
        )
    );
    let usage = completed
        .2
        .expect("provider usage must survive ACP completion");
    assert_eq!(usage.total_tokens, 30);
    assert_eq!(usage.scope, mj_core::usage::UsageScope::Turn);
    assert_eq!(usage.thought_tokens, None);
    let empty_warning = empty_warning.expect("an unanswered turn must warn");
    assert!(
        empty_warning.contains(PROMPT_EMPTY_RESPONSE_MARKER),
        "{empty_warning}"
    );
    assert!(
        empty_warning.contains("may never have been acted on"),
        "{empty_warning}"
    );

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
                ..
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

/// A bridge that accepts a prompt, optionally announces one tool call, and
/// then goes silent for good. This is what a harness blocked in a long build
/// looks like from Mjolnir: an open tool call and no protocol traffic at all.
async fn silent_after_prompt_bridge(
    stream: tokio::io::DuplexStream,
    observed: mpsc::UnboundedSender<String>,
    open_a_tool_call: bool,
) {
    silent_after_prompt_bridge_with_late_reply(stream, observed, open_a_tool_call, false).await;
}

async fn silent_after_prompt_bridge_with_late_reply(
    stream: tokio::io::DuplexStream,
    observed: mpsc::UnboundedSender<String>,
    open_a_tool_call: bool,
    late_reply: bool,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let mut prior_prompt = None;
    while let Some(line) = lines.next_line().await.expect("read bridge input") {
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
                if late_reply && let Some(prior) = prior_prompt.replace(id) {
                    let reply = serde_json::json!({"jsonrpc":"2.0", "id":prior, "result":{"stopReason":"end_turn"}});
                    if write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                if open_a_tool_call {
                    let update = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": "scripted",
                            "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": "long-build",
                                "title": "cargo nextest run",
                                "status": "in_progress",
                            },
                        },
                    });
                    if write
                        .write_all(format!("{update}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Never answer: the turn is now as silent as a harness
                // blocked inside one long tool call.
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
}

pub(super) fn silent_bridge_spec(stall_policy: mj_core::activity::StallPolicy) -> LaunchSpec {
    LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        // Muse marks no turn end of its own, so the watchdog covers it.
        harness: HarnessKind::Muse,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: Some(stall_policy),
    }
}

/// Issue #1020: a harness blocked inside one long tool call sends nothing at
/// all, and the watchdog used to fail the turn for it. The tool call is the
/// sign of life; only its own, much longer bound may end such a turn.
#[tokio::test(flavor = "current_thread")]
async fn a_turn_blocked_in_a_long_tool_call_is_not_failed() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let bridge = tokio::spawn(silent_after_prompt_bridge(bridge_stream, observed_tx, true));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = silent_bridge_spec(mj_core::activity::StallPolicy {
        silence: Some(Duration::from_millis(200)),
        tool_call: Some(Duration::from_secs(3_600)),
    });
    // The relay records the tool call the bridge announced; the watchdog reads
    // the same handle, which is the whole of the fix.
    let tools = spec.tools_in_flight.clone();
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
            prompt: vec![ContentBlock::Text(TextContent::new("build it"))],
        })
        .await
        .unwrap();
    let mut methods = Vec::new();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;
    // Record the tool call the bridge announced, exactly as the worker runtime
    // does when it applies the update to the relay.
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::SessionUpdate { update, .. }
            if update.get("sessionUpdate").and_then(serde_json::Value::as_str) == Some("tool_call"))
    })
    .await;
    tools.open("long-build", mj_core::clock::epoch_millis());

    // Well past the silence bound, and nothing has arrived since the tool call.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let finished = tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await;
    assert!(
        !matches!(
            finished,
            Ok(Some(RuntimeEvent::PromptFinished { .. })) | Ok(Some(RuntimeEvent::Warning { .. }))
        ),
        "a turn blocked in a tool call must not be failed: {finished:?}"
    );

    drop(request_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), driver).await;
    bridge.abort();
}

/// The tool call's own bound is the upper limit that still ends a turn, so a
/// bridge that dies leaving a tool card open cannot hold the turn forever.
/// It could not be forced in a live session — Muse keeps emitting Reminder
/// tool cards, which are activity — so it is covered here.
#[tokio::test(flavor = "current_thread")]
async fn a_tool_call_that_outlives_its_bound_ends_the_turn() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let bridge = tokio::spawn(silent_after_prompt_bridge(bridge_stream, observed_tx, true));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = silent_bridge_spec(mj_core::activity::StallPolicy {
        silence: Some(Duration::from_secs(3_600)),
        tool_call: Some(Duration::from_millis(400)),
    });
    let tools = spec.tools_in_flight.clone();
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
            prompt: vec![ContentBlock::Text(TextContent::new("build it"))],
        })
        .await
        .unwrap();
    let mut methods = Vec::new();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;
    tools.open("long-build", mj_core::clock::epoch_millis());

    let warning = wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::Warning { .. })
    })
    .await;
    let RuntimeEvent::Warning { message } = warning else {
        panic!("expected the stall warning");
    };
    assert!(
        message.contains("long-build"),
        "names the tool call: {message}"
    );
    assert!(
        message.contains("MJ_TURN_TOOL_STALL_TIMEOUT_MS"),
        "names the knob that raises the limit: {message}"
    );

    let finished = wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::PromptFinished { .. })
    })
    .await;
    let RuntimeEvent::PromptFinished { stop_reason, .. } = finished else {
        panic!("expected the turn to be failed");
    };
    assert_eq!(stop_reason, TURN_STALLED_STOP_REASON);

    drop(request_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), driver).await;
    bridge.abort();
}

/// The other half: a harness that really has gone quiet, with nothing in
/// flight, still loses its turn, and the reason travels with the outcome
/// instead of living only in the transcript.
#[tokio::test(flavor = "current_thread")]
async fn a_silent_harness_fails_the_turn_with_a_reason() {
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let bridge = tokio::spawn(silent_after_prompt_bridge(
        bridge_stream,
        observed_tx,
        false,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = silent_bridge_spec(mj_core::activity::StallPolicy {
        silence: Some(Duration::from_millis(200)),
        tool_call: Some(Duration::from_secs(3_600)),
    });
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
            prompt: vec![ContentBlock::Text(TextContent::new("say something"))],
        })
        .await
        .unwrap();
    let mut methods = Vec::new();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;

    let warning = wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::Warning { .. })
    })
    .await;
    let RuntimeEvent::Warning { message } = warning else {
        panic!("expected the stall warning");
    };
    assert!(
        message.contains("no tool call was open"),
        "the transcript says why the short bound applied: {message}"
    );

    let finished = wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::PromptFinished { .. })
    })
    .await;
    let RuntimeEvent::PromptFinished {
        stop_reason,
        diagnostic,
        ..
    } = finished
    else {
        panic!("expected the turn to be failed");
    };
    assert_eq!(stop_reason, TURN_STALLED_STOP_REASON);
    let diagnostic = diagnostic.expect("the outcome carries the reason, not only the transcript");
    assert_eq!(diagnostic.code.as_deref(), Some(TURN_STALLED_STOP_REASON));
    assert!(
        diagnostic.message.contains("stopped responding"),
        "{diagnostic:?}"
    );

    drop(request_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), driver).await;
    bridge.abort();
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
                        "result": {
                            "sessionId": "steering-session",
                            "modes": {
                                "currentModeId": "agent",
                                "availableModes": [
                                    {"id": "agent", "name": "Agent"},
                                    {"id": "agent-full-access", "name": "Full access"},
                                ],
                            },
                        },
                    }),
                    "session/set_mode" => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": {},
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
    exercise_image_steering(false).await;
}

#[tokio::test]
async fn ten_large_photos_reach_acp_for_both_prompt_and_steering() {
    tokio::time::timeout(Duration::from_secs(20), exercise_image_steering(true))
        .await
        .expect("large image delivery must not deadlock");
}

async fn exercise_image_steering(with_images: bool) {
    use base64::Engine as _;
    let images_root = tempfile::tempdir().unwrap();
    let store = mj_core::attachment::AttachmentStore::worker(images_root.path());
    let mut image_blocks = Vec::new();
    let mut expected_images = Vec::new();
    if with_images {
        for index in 0..10 {
            let mut bytes = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
            bytes.resize(mj_core::attachment::MAX_IMAGE_BYTES, index);
            let reference =
                mj_core::attachment::AttachmentRef::new(&bytes, "image/png".into(), 1, 1).unwrap();
            store.install(&reference, &bytes).unwrap();
            image_blocks.push(reference.content_block());
            expected_images.push(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    let blocks = |text: &str| {
        let mut blocks = vec![ContentBlock::Text(TextContent::new(text))];
        blocks.extend(image_blocks.clone());
        blocks
    };
    let assert_images = |request: &serde_json::Value| {
        let prompt = request["params"]["prompt"].as_array().unwrap();
        assert_eq!(prompt.len(), expected_images.len() + 1);
        for (image, expected) in prompt.iter().skip(1).zip(&expected_images) {
            assert_eq!(image["data"].as_str(), Some(expected.as_str()));
            assert!(image["uri"].is_null());
        }
    };
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (complete_tx, complete_rx) = mpsc::channel(1);
    let bridge = tokio::spawn(steering_bridge(bridge_stream, observed_tx, complete_rx));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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

    assert!(matches!(
        event_rx.recv().await,
        Some(RuntimeEvent::Connected {
            steering_supported: Some(true),
            ..
        }),
    ));

    request_tx
        .send(if with_images {
            CommandRequest::PromptAttachments {
                request_id: "prompt-1".into(),
                prompt: blocks("start"),
                root: images_root.path().to_path_buf(),
            }
        } else {
            CommandRequest::Prompt {
                request_id: "prompt-1".into(),
                prompt: blocks("start"),
            }
        })
        .await
        .unwrap();
    loop {
        let request = observed_rx.recv().await.expect("prompt reaches bridge");
        if request["method"] == "session/prompt" {
            assert_images(&request);
            break;
        }
    }
    request_tx
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: Some(ClaimedSteeringPrompt {
                attachment_root: with_images.then(|| images_root.path().to_path_buf()),
                queued_command_id: "queued-1".into(),
                prompt: blocks("change direction"),
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
    assert_images(&steering);
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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
    assert_eq!(restart, Some(SessionRestart::Resume("scripted".into())));
    bridge.abort();
}

#[tokio::test(start_paused = true)]
async fn a_request_queued_across_a_restart_never_reaches_the_fresh_bridge() {
    fn scripted_spec(resume_session: Option<String>) -> LaunchSpec {
        LaunchSpec {
            bridge_spec_path: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "scripted".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session,
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness: HarnessKind::Kimi,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
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
    assert_eq!(restart, Some(SessionRestart::Resume("scripted".into())));
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
        driver: tokio::task::JoinHandle<Result<Option<SessionRestart>>>,
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
            bridge_spec_path: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "scripted".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: None,
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness: HarnessKind::Kimi,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
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
async fn unused_codex_bridge_restarts_with_a_new_thread() {
    codex_bridge_restart_preserves_only_used_threads(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn codex_bridge_restarts_with_the_used_thread_even_if_the_prompt_reply_was_lost() {
    codex_bridge_restart_preserves_only_used_threads(true).await;
}

#[cfg(unix)]
async fn codex_bridge_restart_preserves_only_used_threads(send_prompt: bool) {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("restarted");
    let script = temp.path().join("codex_persistence.py");
    std::fs::write(&script, format!(r#"
import json, os, sys, time
marker = {marker:?}
used = {send_prompt}
second = os.path.exists(marker)

def write(payload):
    print(json.dumps(payload), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    ident = request.get("id")
    method = request.get("method")
    if method == "initialize":
        result = {{"protocolVersion": 1, "agentCapabilities": {{"loadSession": True}}, "_meta": {{"jetbrains": {{"air": {{"version": 1, "capabilities": ["nativeSubagentSessions"]}}}}}}}}
    elif method in ("session/new", "session/load", "session/resume"):
        if second and not used and method != "session/new":
            write({{"jsonrpc": "2.0", "id": ident, "error": {{"code": -32603, "message": "no rollout found for thread id unused"}}}})
            continue
        assert method == ("session/load" if second and used else "session/new"), request
        result = {{"sessionId": "original" if not second or used else "replacement",
                   "modes": {{"currentModeId": "agent", "availableModes": [{{"id": "agent", "name": "Guardian"}}]}}}}
        write({{"jsonrpc": "2.0", "id": ident, "result": result}})
        continue
    elif method == "session/prompt":
        assert not second and used
        open(marker, "w").close()
        # Crash after consuming the prompt, before replying.
        break
    else:
        result = {{}}
    if ident is not None:
        write({{"jsonrpc": "2.0", "id": ident, "result": result}})
    if method == "session/set_mode" and not second and not used:
        open(marker, "w").close()
        time.sleep(0.2)
        break
"#, send_prompt = if send_prompt { "True" } else { "False" })).unwrap();
    let (request_tx, request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let runtime = tokio::spawn(run(
        LaunchSpec {
            bridge_spec_path: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            environment: BTreeMap::new(),
            cwd: temp.path().to_path_buf(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: None,
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness: HarnessKind::Codex,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
        },
        request_rx,
        event_tx,
    ));
    let mut opened = Vec::new();
    let mut prompt_sent = false;
    let mut replay = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), event_rx.recv())
            .await
            .expect("the replacement bridge must become ready")
            .expect("the runtime must survive the first bridge's death");
        match event {
            RuntimeEvent::NativeAgent { event } => replay.push(event),
            RuntimeEvent::SessionStarted {
                native_session_id,
                resumed,
                ..
            } => {
                opened.push((native_session_id, resumed));
            }
            RuntimeEvent::SessionConfigured { .. } => {
                if opened.len() == 2 {
                    break;
                }
                if send_prompt && !prompt_sent {
                    request_tx
                        .send(CommandRequest::Prompt {
                            request_id: "prompt-1".into(),
                            prompt: vec![ContentBlock::Text(TextContent::new("do work"))],
                        })
                        .await
                        .unwrap();
                    prompt_sent = true;
                }
            }
            RuntimeEvent::Stopped => {
                panic!("runtime stopped instead of recovering: {:?}", runtime.await)
            }
            _ => {}
        }
    }
    assert_eq!(
        opened,
        vec![
            ("original".into(), false),
            (
                if send_prompt {
                    "original"
                } else {
                    "replacement"
                }
                .into(),
                send_prompt
            ),
        ]
    );
    assert_eq!(
        replay,
        if send_prompt {
            vec![
                mj_core::native_agent::NativeAgentEvent::ReplayBegin,
                mj_core::native_agent::NativeAgentEvent::ReplayCommit,
            ]
        } else {
            Vec::new()
        }
    );
    drop(request_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// A replacement bridge is a fresh harness process, and Codex builds the
/// resumed thread's MCP set out of the `session/resume` request alone, so the
/// delegation server has to be stated again at every launch. Without this the
/// session's `mcp__mj_agents__*` tools disappeared after the first daemon or
/// bridge restart (#1085).
#[cfg(unix)]
#[tokio::test]
async fn a_relaunched_codex_session_keeps_delegation_and_memory_tools() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("second-bridge");
    let opens = temp.path().join("opens.txt");
    let script = temp.path().join("restarting_codex.py");
    std::fs::write(
        &script,
        format!(
            r#"
import json, os, sys, time
marker = {marker:?}
opens = {opens:?}

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

second = os.path.exists(marker)
mode = {{"id": "interaction_mode", "name": "Mode", "category": "mode",
        "type": "select", "currentValue": "agent",
        "options": [{{"value": "agent", "name": "Agent"}}]}}
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get("method"), request.get("id")
    if ident is None:
        continue
    if method == "initialize":
        write({{"jsonrpc": "2.0", "id": ident, "result": {{
            "protocolVersion": 1,
            "agentCapabilities": {{"loadSession": True,
                                  "sessionCapabilities": {{"resume": {{}}}}}}}}}})
        continue
    if method in ("session/new", "session/resume", "session/load"):
        servers = request.get("params", {{}}).get("mcpServers")
        names = "none" if servers is None else ",".join(
            server["name"] for server in servers)
        with open(opens, "a") as log:
            log.write(method + " " + names + "\n")
        write({{"jsonrpc": "2.0", "id": ident, "result": {{
            "sessionId": "scripted", "configOptions": [mode],
            "modes": {{"currentModeId": "agent",
                      "availableModes": [{{"id": "agent", "name": "Agent"}}]}}}}}})
        continue
    if method == "session/set_config_option":
        write({{"jsonrpc": "2.0", "id": ident, "result": {{"configOptions": [mode]}}}})
        continue
    if method == "session/prompt":
        if not second:
            # The bridge dies on a thread that has been used, the way it does
            # when the daemon restarts underneath a live session.
            open(marker, "w").close()
            time.sleep(0.2)
            break
        write({{"jsonrpc": "2.0", "method": "session/update", "params": {{
            "sessionId": "scripted",
            "update": {{"sessionUpdate": "agent_message_chunk",
                       "content": {{"type": "text", "text": "ok"}}}}}}}})
        write({{"jsonrpc": "2.0", "id": ident, "result": {{"stopReason": "end_turn"}}}})
        continue
    write({{"jsonrpc": "2.0", "id": ident, "result": {{}}}})
"#,
        ),
    )
    .unwrap();

    let (request_tx, request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: Some(temp.path().join("subagents.sock")),
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: Some(ProjectMemoryLaunchConfig {
            history_socket: None,
            project_key: "project".into(),
            root: temp.path().join("memory"),
            baseline_root: temp.path().join("baseline"),
            repository_roots: BTreeMap::new(),
            mcp_delivery: ProjectMemoryMcpDelivery::Acp,
        }),
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Codex,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };
    let runtime = tokio::spawn(run(spec, request_rx, event_tx));

    let mut started = Vec::new();
    let mut prompt_sent = false;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), event_rx.recv())
            .await
            .expect("ACP runtime keeps reporting")
            .expect("event channel stays open");
        match event {
            RuntimeEvent::SessionStarted { resumed, .. } => started.push(resumed),
            RuntimeEvent::SessionConfigured { .. } => {
                if started.len() == 2 {
                    break;
                }
                if !prompt_sent {
                    // A thread is only resumed once it has been used.
                    request_tx
                        .send(CommandRequest::Prompt {
                            request_id: "prompt-1".into(),
                            prompt: vec![ContentBlock::Text(TextContent::new("do work"))],
                        })
                        .await
                        .unwrap();
                    prompt_sent = true;
                }
            }
            RuntimeEvent::Stopped => panic!("the worker stopped instead of relaunching"),
            _ => {}
        }
    }
    assert_eq!(started, vec![false, true], "the second open is a resume");

    drop(request_tx);
    tokio::time::timeout(std::time::Duration::from_secs(10), runtime)
        .await
        .expect("closing the command channel must end the runtime")
        .expect("runtime task does not panic")
        .expect("a relaunched bridge must not fail the worker");

    assert_eq!(
        std::fs::read_to_string(&opens).unwrap(),
        "session/new mj-agents,mj-memory\nsession/resume mj-agents,mj-memory\n",
        "every launch must carry delegation and memory servers"
    );
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
            assert [server['name'] for server in servers] == ['mj-memory'], request
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: Some(ProjectMemoryLaunchConfig {
            history_socket: None,
            project_key: "abc".into(),
            root: temp.path().join("memory"),
            baseline_root: temp.path().join("baseline"),
            repository_roots: BTreeMap::new(),
            mcp_delivery: ProjectMemoryMcpDelivery::Acp,
        }),
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };
    let runtime = tokio::spawn(run(spec, request_rx, event_tx));

    let mut started = Vec::new();
    let mut saw_reload = false;
    let mut capability_updates = 0;
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
                assert_eq!(
                    update,
                    serde_json::json!({"sessionUpdate":"session_info_update", "_meta":{"mjGoalCapability":null}}),
                    "resume replay leaked into the relay: {update}"
                );
                capability_updates += 1;
            }
            RuntimeEvent::Stopped => panic!("worker stopped before reloading the session"),
            _ => {}
        }
    }
    assert_eq!(
        capability_updates, 2,
        "each bridge refreshes its goal controls"
    );
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: temp.path().to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
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
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
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
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::Unconstrained,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    };

    // `run` spawns a real `sh` and waits on its real stdout, and its own
    // cleanup path wraps `child.wait()` in a five-second `tokio::time::timeout`
    // that a paused clock would auto-advance past while that child is still
    // alive. So this test keeps a real clock, and the guard below only stops a
    // hang from becoming an unbounded one: the assertions are all on the error
    // text, none on how long the call took, so it can be generous enough for a
    // machine running other builds.
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(120),
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
    for (bridge, cwd) in [
        (
            temp.path().join("missing-acp-bridge"),
            temp.path().to_path_buf(),
        ),
        (PathBuf::from("sh"), temp.path().join("missing-checkout")),
    ] {
        let (_request_tx, request_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let spec = LaunchSpec {
            bridge_spec_path: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: bridge.clone(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: cwd.clone(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: None,
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness: HarnessKind::Kimi,
            execution_policy: ExecutionPolicy::Unconstrained,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
        };

        let error = run(spec, request_rx, event_tx).await.unwrap_err();
        assert!(format!("{error:#}").contains(&format!("launch ACP bridge {}", bridge.display())));
        assert!(format!("{error:#}").contains(&format!("working directory {}", cwd.display())));
        assert!(matches!(
            event_rx.recv().await,
            Some(RuntimeEvent::Warning { message }) if message.contains("ACP runtime failed")
        ));
        assert!(matches!(event_rx.recv().await, Some(RuntimeEvent::Stopped)));
    }
}

#[test]
fn echoed_user_images_do_not_reenter_the_relay_journal() {
    use agent_client_protocol::schema::v1::{ContentChunk, ImageContent};
    let update = SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Image(
        ImageContent::new("x".repeat(700 * 1024), "image/png"),
    )));
    assert!(!session_update_is_relay_visible(
        &update,
        &Mutex::new(BTreeSet::new()),
        "session-1"
    ));
}

#[test]
fn an_out_of_spec_tool_status_is_coerced_to_failed_so_the_card_settles() {
    // Muse's adapter can send an ACP-illegal `cancelled` status; the whole
    // notification used to be dropped, stranding the tool card in_progress.
    let mut update = serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "item-1",
        "status": "cancelled",
    });
    let replaced = coerce_tool_call_status(&mut update);
    assert_eq!(replaced.as_deref(), Some("cancelled"));
    assert_eq!(update["status"], "failed");
    // And it now parses into a real v1 update rather than being discarded.
    serde_json::from_value::<SessionUpdate>(update).expect("coerced update parses");
}

#[test]
fn a_legal_tool_status_and_a_non_tool_update_are_left_untouched() {
    let mut legal = serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "item-1",
        "status": "in_progress",
    });
    assert_eq!(coerce_tool_call_status(&mut legal), None);
    assert_eq!(legal["status"], "in_progress");

    let mut message = serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": "hi"},
    });
    assert_eq!(coerce_tool_call_status(&mut message), None);
}

#[test]
fn salvage_settles_a_named_tool_and_ignores_the_rest() {
    // When a tool update cannot be represented at all, we still settle the
    // named tool as failed rather than drop it and strand the card.
    let raw = serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "item-2",
        "status": "completed",
        "content": [{"type": "from_the_future", "payload": 7}],
    });
    let salvaged = salvage_tool_call_update(&raw).expect("a named tool update is salvageable");
    match salvaged {
        SessionUpdate::ToolCallUpdate(update) => {
            assert_eq!(update.tool_call_id.0.as_ref(), "item-2");
        }
        other => panic!("expected a tool_call_update, got {other:?}"),
    }

    // Without a tool id there is nothing to settle, and a non-tool update is
    // left for the caller to drop.
    let no_id = serde_json::json!({"sessionUpdate": "tool_call_update", "status": "completed"});
    assert!(salvage_tool_call_update(&no_id).is_none());
    let message = serde_json::json!({"sessionUpdate": "agent_message_chunk"});
    assert!(salvage_tool_call_update(&message).is_none());
}

/// Both stall bounds are off unless an operator asks for one.
///
/// Mjolnir does not guess that a quiet turn is a dead turn: silence is not
/// evidence, and failing a healthy turn for it destroys real work (#1020,
/// #1017). Only a positive number of milliseconds arms a bound, so an unset,
/// empty, zero or mistyped value leaves the turn running and visible.
#[test]
fn a_stall_bound_is_off_unless_a_positive_timeout_is_configured() {
    for off in [
        None,
        Some(""),
        Some("  "),
        Some("0"),
        Some("off"),
        Some("-5"),
    ] {
        assert_eq!(
            parse_stall_timeout(off),
            None,
            "{off:?} must not arm a watchdog"
        );
    }
    assert_eq!(
        parse_stall_timeout(Some(" 30000 ")),
        Some(Duration::from_millis(30_000)),
        "a positive value arms the bound it names"
    );
}

#[test]
fn the_stall_message_says_what_happened_and_what_to_do() {
    let silent = turn_stall_message(
        HarnessKind::Muse,
        &mj_core::activity::StallVerdict::Silent { silent_ms: 630_000 },
    );
    assert!(silent.contains("stopped responding"));
    assert!(
        silent.contains("10 minute"),
        "reports the silence in minutes: {silent}"
    );
    assert!(
        silent.contains("no tool call was open"),
        "says why the short bound applied: {silent}"
    );
    assert!(silent.contains("Resend"), "tells the user how to continue");
    assert!(silent.contains("#1007"), "names the known issue");

    // A turn ended for one long tool call must name the call, how long it
    // ran, and the knob that raises or removes the limit.
    let tool = turn_stall_message(
        HarnessKind::Muse,
        &mj_core::activity::StallVerdict::ToolCall {
            tool_call_id: "job_output-7".into(),
            running_ms: 14_460_000,
            silent_ms: 14_400_000,
        },
    );
    assert!(tool.contains("job_output-7"), "names the tool call: {tool}");
    assert!(tool.contains("241 minute"), "how long it ran: {tool}");
    assert!(
        tool.contains("MJ_TURN_TOOL_STALL_TIMEOUT_MS"),
        "names the knob: {tool}"
    );
}

/// The message tells the user which variable raises the limit, and the code
/// has to read that same variable. It did not: the lookup said
/// `MJ_TURN_TOOL_CALL_TIMEOUT_MS` while the message, the documentation and the
/// daemon's passthrough all said `MJ_TURN_TOOL_STALL_TIMEOUT_MS`, so setting
/// the documented variable did nothing and the bound was always the four-hour
/// default. Nothing caught it because every test asserted on the message.
#[test]
fn the_tool_call_bound_reads_the_variable_its_message_advertises() {
    let message = turn_stall_message(
        HarnessKind::Muse,
        &mj_core::activity::StallVerdict::ToolCall {
            tool_call_id: "bash-1".into(),
            running_ms: 1,
            silent_ms: 1,
        },
    );
    assert!(
        message.contains(TOOL_CALL_STALL_TIMEOUT_VARIABLE),
        "the message names the variable the code reads: {message}"
    );

    // And the daemon carries that same name to the workers it starts, which
    // is the only way it reaches a worker at all.
    let carried = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mj-controller/src/controller/worker_binary/launch.rs"
    ))
    .expect("read the launch configuration that carries the knob");
    assert!(
        carried.contains(TOOL_CALL_STALL_TIMEOUT_VARIABLE),
        "the daemon carries the variable the worker reads"
    );
}

/// The daemon carries both knobs to the workers it starts. A worker re-execs
/// with a cleared environment, so a value set for the daemon reaches it only
/// because this list names it. Without the silence knob in that list there is
/// no way to arm the opt-in bound on any target.
#[test]
fn the_daemon_carries_both_stall_knobs_to_its_workers() {
    let carried = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mj-controller/src/controller/worker_binary/launch.rs"
    ))
    .expect("read the launch configuration that carries the knobs");
    for name in [
        TURN_STALL_TIMEOUT_VARIABLE,
        TOOL_CALL_STALL_TIMEOUT_VARIABLE,
        "TYPESAFE_API_KEY",
    ] {
        assert!(
            carried.contains(name),
            "the daemon must carry {name} to the worker that reads it"
        );
    }
}

/// Fake bridge that rejects every attempt to reload a recorded session and
/// answers `session/new` with a fresh id.
async fn session_reload_rejecting_bridge(
    stream: tokio::io::DuplexStream,
    observed: mpsc::UnboundedSender<String>,
    advertised_resume: bool,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await.expect("read fake adapter input") {
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("fake adapter input is JSON-RPC");
        let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let _ = observed.send(method.to_owned());
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match method {
            "initialize" => {
                let mut result = serde_json::json!({"protocolVersion": 1});
                if advertised_resume {
                    result["agentCapabilities"] = serde_json::json!({
                        "sessionCapabilities": {"resume": {}}
                    });
                }
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            "session/load" | "session/resume" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Session not found: it no longer exists in the backend",
                },
            }),
            "session/new" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "sessionId": "fresh",
                    "modes": {
                        "currentModeId": "build",
                        "availableModes": [{"id": "build", "name": "Build"}],
                    },
                },
            }),
            // Anything else session setup sends (mode or config selection)
            // succeeds trivially.
            _ if !id.is_null() => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
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

fn reload_fallback_spec(harness: HarnessKind) -> LaunchSpec {
    LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "scripted".into(),
        args: Vec::new(),
        environment: BTreeMap::new(),
        cwd: std::env::current_dir().unwrap(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: Some("gone".into()),
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        harness,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    }
}

#[tokio::test]
async fn codex_still_fails_when_the_recorded_session_cannot_be_reloaded() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let bridge = tokio::spawn(session_reload_rejecting_bridge(
        bridge_stream,
        observed_tx,
        false,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (_request_tx, mut request_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(32);
    let driver = tokio::spawn(async move {
        drive(
            transport,
            reload_fallback_spec(HarnessKind::Codex),
            &mut request_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });

    let error = tokio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("a failed load ends the runtime")
        .expect("the runtime task does not panic")
        .expect_err("a harness with restorable native state must not start fresh");
    assert!(
        format!("{error:#}").contains("load ACP session gone"),
        "unexpected error: {error:#}"
    );
    drop(event_rx.try_recv());
    let mut methods = Vec::new();
    while let Ok(method) = observed_rx.try_recv() {
        if matches!(
            method.as_str(),
            "session/new" | "session/load" | "session/resume"
        ) {
            methods.push(method);
        }
    }
    assert_eq!(methods, vec!["session/load".to_owned()]);
    bridge.abort();
}

#[test]
fn resume_failures_report_a_missing_native_session_per_harness() {
    fn spec(harness: HarnessKind) -> LaunchSpec {
        LaunchSpec {
            bridge_spec_path: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "agent".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: "/workspace/app".into(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: Some("native".into()),
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: crate::acp::StepClock::default(),
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
        }
    }
    // The message as codex-acp wraps it.
    let missing_thread = anyhow::anyhow!(
        r#"Internal error: {{"details": "no rollout found for thread id native"}}"#
    )
    .context("resume ACP session native");
    // The ACP standard resource error as claude-agent-acp answers it.
    let missing_transcript =
        anyhow::anyhow!("Resource not found: native: {{\n  \"uri\": \"native\"\n}}")
            .context("resume ACP session native");
    assert!(harness_reports_missing_native_session(
        &spec(HarnessKind::Codex),
        &missing_thread
    ));
    assert!(harness_reports_missing_native_session(
        &spec(HarnessKind::Claude),
        &missing_transcript
    ));
    // Each harness only recognizes its own wording.
    assert!(!harness_reports_missing_native_session(
        &spec(HarnessKind::Claude),
        &missing_thread
    ));
    assert!(!harness_reports_missing_native_session(
        &spec(HarnessKind::Codex),
        &missing_transcript
    ));
    // A harness that always materializes its session never reports one gone.
    assert!(!harness_reports_missing_native_session(
        &spec(HarnessKind::Kimi),
        &missing_thread
    ));
    // Any other failure keeps failing the resume.
    assert!(!harness_reports_missing_native_session(
        &spec(HarnessKind::Codex),
        &anyhow::anyhow!("Internal error: session store is locked")
    ));
    assert!(!harness_reports_missing_native_session(
        &spec(HarnessKind::Claude),
        &anyhow::anyhow!("resume ACP session native: connection closed")
    ));
}

/// A fake ACP bridge that refuses every reload of `missing-thread` with
/// `reload_error`, the text the harness under test really answers, and opens a
/// fresh session for `session/new`.
#[cfg(unix)]
fn missing_native_session_script(directory: &Path, reload_error: &str) -> std::path::PathBuf {
    let script = directory.join("missing_native_session.py");
    let source = r#"
import json, sys

RELOAD_ERROR = __RELOAD_ERROR__

for line in sys.stdin:
    request = json.loads(line)
    ident = request.get("id")
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}
    elif method in ("session/load", "session/resume"):
        print(json.dumps({"jsonrpc": "2.0", "id": ident,
                          "error": {"code": -32603, "message": RELOAD_ERROR}}),
              flush=True)
        continue
    elif method == "session/new":
        # Both harnesses' guardian modes, so either can select its own.
        result = {"sessionId": "replacement",
                  "modes": {"currentModeId": "agent",
                            "availableModes": [{"id": "agent", "name": "Guardian"},
                                               {"id": "auto", "name": "Auto"}]}}
    elif method == "session/prompt":
        result = {"stopReason": "end_turn"}
    else:
        result = {}
    if ident is not None:
        print(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}), flush=True)
"#;
    // A JSON string literal is also a Python string literal, so the error text
    // reaches the script exactly as the harness would send it.
    std::fs::write(
        &script,
        source.replace(
            "__RELOAD_ERROR__",
            &serde_json::to_string(reload_error).unwrap(),
        ),
    )
    .unwrap();
    script
}

/// Codex's wrapped refusal for a thread whose rollout was never written.
#[cfg(unix)]
const MISSING_CODEX_THREAD_ERROR: &str =
    r#"Internal error: {"details": "no rollout found for thread id missing-thread"}"#;

/// Claude Code's ACP resource error for a session with no transcript on disk.
#[cfg(unix)]
const MISSING_CLAUDE_SESSION_ERROR: &str =
    "Resource not found: missing-thread: {\n  \"uri\": \"missing-thread\"\n}";

#[cfg(unix)]
fn missing_native_session_spec(
    directory: &Path,
    script: &Path,
    harness: HarnessKind,
    native_session_may_have_history: bool,
) -> LaunchSpec {
    LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: directory.to_path_buf(),
        additional_directories: Vec::new(),
        extra_mcp_servers: Vec::new(),
        project_memory: None,
        resume_session: Some("missing-thread".into()),
        native_session_may_have_history,
        accepted_config: Default::default(),
        harness,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: crate::acp::StepClock::default(),
        tools_in_flight: Default::default(),
        turn_context: Default::default(),
        verdict: Some(crate::acp::VerdictSource::Direct {
            key: String::new(),
            endpoint: String::new(),
        }),
        stall_policy: None,
    }
}

/// Drive the runtime against a bridge that cannot find `missing-thread`, with
/// Mjolnir's state showing the native session was never used, and prove the
/// queued work finishes on a replacement session in the same Mjolnir session.
#[cfg(unix)]
async fn assert_unused_native_session_is_replaced(harness: HarnessKind, reload_error: &str) {
    let temp = tempfile::tempdir().unwrap();
    let script = missing_native_session_script(temp.path(), reload_error);
    let (request_tx, request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    // Work the worker queued before the session opened must survive the
    // replacement and run on the new thread.
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "queued-prompt".into(),
            prompt: vec![ContentBlock::Text(TextContent::new("do the queued work"))],
        })
        .await
        .unwrap();
    let runtime = tokio::spawn(run(
        missing_native_session_spec(temp.path(), &script, harness, false),
        request_rx,
        event_tx,
    ));

    let mut warnings = Vec::new();
    let mut opened = None;
    let mut finished = None;
    while finished.is_none() {
        let event = tokio::time::timeout(Duration::from_secs(10), event_rx.recv())
            .await
            .expect("the replacement thread must open")
            .expect("the runtime must keep serving the session");
        match event {
            RuntimeEvent::Warning { message } => warnings.push(message),
            RuntimeEvent::SessionStarted {
                native_session_id,
                resumed,
                ..
            } => opened = Some((native_session_id, resumed)),
            RuntimeEvent::PromptFinished { request_id, .. } => finished = Some(request_id),
            RuntimeEvent::Stopped => {
                panic!("the runtime stopped instead of replacing the session: {warnings:?}")
            }
            _ => {}
        }
    }
    assert_eq!(opened, Some(("replacement".into(), false)));
    assert_eq!(finished.as_deref(), Some("queued-prompt"));
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("missing-thread")
                && warning.contains("new empty session")
                && warning.contains(harness.display_name())),
        "the replacement must be reported: {warnings:?}"
    );
    drop(request_tx);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn an_unused_codex_thread_codex_cannot_find_is_replaced_in_the_same_session() {
    assert_unused_native_session_is_replaced(HarnessKind::Codex, MISSING_CODEX_THREAD_ERROR).await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_unused_claude_session_claude_cannot_find_is_replaced_in_the_same_session() {
    assert_unused_native_session_is_replaced(HarnessKind::Claude, MISSING_CLAUDE_SESSION_ERROR)
        .await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_used_codex_thread_codex_cannot_find_fails_instead_of_starting_over() {
    let temp = tempfile::tempdir().unwrap();
    let script = missing_native_session_script(temp.path(), MISSING_CODEX_THREAD_ERROR);
    let (request_tx, request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let runtime = tokio::spawn(run(
        missing_native_session_spec(temp.path(), &script, HarnessKind::Codex, true),
        request_rx,
        event_tx,
    ));

    let error = tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .expect("the runtime must fail rather than hang")
        .unwrap()
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("no native history for session missing-thread"),
        "unexpected failure: {error}"
    );
    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::SessionStarted { .. })),
        "a used thread must never be replaced: {events:?}"
    );
    drop(request_tx);
}

/// Exercise the real session select loop and the production 60-second cadence.
#[tokio::test(flavor = "current_thread")]
async fn classifier_marks_a_quiet_prompt_as_awaiting_input_without_closing_the_session() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut length = 0;
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        let evidence: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(evidence["state"]["phase"], "running");
        let answer = serde_json::json!({"answers": {
            "waiting_on":{"type":"choice","choice":"user","confidence":0.95},
            "asked_question":{"type":"noul","noul":0.95}
        }})
        .to_string();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let bridge = tokio::spawn(silent_after_prompt_bridge_with_late_reply(
        bridge_stream,
        observed_tx,
        false,
        true,
    ));
    let (client_read, client_write) = tokio::io::split(client_stream);
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (request_tx, mut request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let mut spec = silent_bridge_spec(mj_core::activity::StallPolicy {
        silence: None,
        tool_call: None,
    });
    spec.verdict = Some(VerdictSource::Direct {
        key: "test-key".into(),
        endpoint,
    });
    let mut driver = tokio::spawn(async move {
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
            prompt: vec![ContentBlock::from("Ask me which option to use")],
        })
        .await
        .unwrap();
    let mut methods = Vec::new();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;
    let (warned, diagnostic) = tokio::time::timeout(Duration::from_secs(75), async {
        let mut warned = false;
        loop {
            match event_rx.recv().await.unwrap() {
                RuntimeEvent::Warning { message } => warned |= message.contains("waiting for you"),
                RuntimeEvent::PromptFinished {
                    stop_reason,
                    diagnostic,
                    ..
                } => {
                    assert_eq!(stop_reason, mj_core::acp::AWAITING_INPUT_STOP_REASON);
                    break (warned, diagnostic.unwrap());
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(warned, "warning precedes completion");
    assert_eq!(
        diagnostic.code.as_deref(),
        Some(mj_core::acp::AWAITING_INPUT_STOP_REASON)
    );
    // A new prompt still reaches the same bridge after dropping the old reply.
    request_tx
        .send(CommandRequest::Prompt {
            request_id: "prompt-2".into(),
            prompt: vec![ContentBlock::from("Use option one")],
        })
        .await
        .unwrap();
    wait_for_bridge_prompt(&mut observed_rx, &mut methods).await;
    assert!(!driver.is_finished());
    let late = tokio::time::timeout(Duration::from_millis(150), async {
        while let Some(event) = event_rx.recv().await {
            if matches!(event, RuntimeEvent::PromptFinished { .. }) {
                return event;
            }
        }
        panic!("session closed after a late reply");
    })
    .await;
    assert!(
        late.is_err(),
        "the old reply must not complete the new prompt"
    );
    drop(request_tx);
    if tokio::time::timeout(Duration::from_secs(5), &mut driver)
        .await
        .is_err()
    {
        driver.abort();
    }
    bridge.abort();
    server.await.unwrap();
}

#[tokio::test]
async fn native_child_load_negotiates_and_routes_history_for_claude_and_codex() {
    use mj_core::native_agent::NativeAgentEvent;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for (harness, missing) in [
        (HarnessKind::Claude, false),
        (HarnessKind::Codex, false),
        (HarnessKind::Codex, true),
    ] {
        let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
        let bridge = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(bridge_stream);
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap_or("");
                let result = match method {
                    "initialize" => {
                        assert!(request["params"]["clientCapabilities"]["_meta"]["jetbrains"]["air"]["capabilities"]
                            .as_array().unwrap().iter().any(|v| v == "nativeSubagentSessions"));
                        serde_json::json!({"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"resume":{}}},
                            "_meta":{"jetbrains":{"air":{"version":1,"capabilities":["nativeSubagentSessions"]}}}})
                    }
                    "session/load" if missing => {
                        let response = serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"no rollout found for thread id root"}});
                        write
                            .write_all(format!("{response}\n").as_bytes())
                            .await
                            .unwrap();
                        continue;
                    }
                    "session/load" | "session/new" => {
                        // Exceed the pipe capacity so the driver must drain history
                        // concurrently with the load response.
                        for (address, update) in [
                            (
                                "root",
                                serde_json::json!({"sessionUpdate":"subagent_spawned","subagentSessionId":"child","name":"review","task":"inspect","capabilities":{}}),
                            ),
                            (
                                "child",
                                serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x".repeat(80000)}}),
                            ),
                            (
                                "root",
                                serde_json::json!({"sessionUpdate":"subagent_state_update","subagentSessionId":"child","state":"completed"}),
                            ),
                        ] {
                            let notification = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":address,"update":update}});
                            write
                                .write_all(format!("{notification}\n").as_bytes())
                                .await
                                .unwrap();
                        }
                        serde_json::json!({"sessionId":"replacement","modes":{"currentModeId":"auto","availableModes":[{"id":"auto","name":"Auto"},{"id":"agent","name":"Agent"}]}})
                    }
                    "session/resume" => panic!("native recovery must load child history"),
                    "session/set_mode" | "session/set_config_option" => serde_json::json!({}),
                    _ => continue,
                };
                let response =
                    serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result});
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (client_read, client_write) = tokio::io::split(client_stream);
        let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let step_clock = crate::acp::StepClock::default();
        let spec = LaunchSpec {
            bridge_spec_path: None,
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "scripted".into(),
            args: Vec::new(),
            environment: BTreeMap::new(),
            cwd: std::env::current_dir().unwrap(),
            additional_directories: Vec::new(),
            extra_mcp_servers: Vec::new(),
            project_memory: None,
            resume_session: Some("root".into()),
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock,
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
        let mut children = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = event_rx.recv().await {
                match event {
                    RuntimeEvent::NativeAgent { event } => {
                        children.push(event);
                    }
                    RuntimeEvent::SessionConfigured { .. } => break,
                    RuntimeEvent::SessionUpdate { update } => {
                        assert_ne!(update["sessionUpdate"], "agent_message_chunk")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("child history load finishes");
        assert_eq!(children.len(), 5);
        assert!(matches!(children[0], NativeAgentEvent::ReplayBegin));
        let spawn_index = if missing {
            assert!(matches!(children[1], NativeAgentEvent::ReplayCommit));
            2
        } else {
            assert!(matches!(children[4], NativeAgentEvent::ReplayCommit));
            1
        };
        assert!(matches!(
            children[spawn_index],
            NativeAgentEvent::Spawned { .. }
        ));
        assert!(matches!(
            children[spawn_index + 1],
            NativeAgentEvent::Update { .. }
        ));
        assert!(matches!(
            children[spawn_index + 2],
            NativeAgentEvent::State { .. }
        ));
        drop(request_tx);
        driver.await.unwrap().unwrap();
        bridge.abort();
    }
}

#[tokio::test]
async fn clear_replaces_the_native_session_without_forwarding_a_prompt() {
    exercise_context_clear(false, HarnessKind::Codex).await;
    exercise_context_clear(false, HarnessKind::Claude).await;
}

#[tokio::test]
async fn failed_clear_reloads_the_previous_native_session() {
    exercise_context_clear(true, HarnessKind::Codex).await;
    exercise_context_clear(true, HarnessKind::Claude).await;
}

async fn exercise_context_clear(fail_replacement: bool, harness: HarnessKind) {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("clear.py");
    std::fs::write(&script, r#"
import json, os, sys
root = sys.argv[1]
fail = sys.argv[2] == 'true'
count_path = os.path.join(root, 'generation')
generation = int(open(count_path).read()) + 1 if os.path.exists(count_path) else 1
with open(count_path, 'w') as f: f.write(str(generation))
options = [{'id':'fast','name':'Fast','type':'select','currentValue':'true' if generation == 1 else 'false','options':[{'value':'true','name':'On'},{'value':'false','name':'Off'}]}]
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    ident = request.get('id')
    with open(os.path.join(root, 'requests.jsonl'), 'a') as f:
        f.write(json.dumps(request) + '\n')
    if ident is None: continue
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True}}
    elif method in ('session/new', 'session/load', 'session/resume'):
        if generation == 2 and fail:
            print(json.dumps({'jsonrpc':'2.0','id':ident,'error':{'code':-32603,'message':'replacement refused'}}), flush=True)
            continue
        result = {'sessionId': 'original' if generation == 1 or method != 'session/new' else 'replacement',
                  'configOptions':options,
                  'modes': {'currentModeId':'plan' if generation == 1 else 'agent','availableModes':[{'id':'agent','name':'Agent'},{'id':'plan','name':'Plan'},{'id':'auto','name':'Auto'}]}}
    elif method == 'session/set_config_option':
        options[0]['currentValue'] = request['params']['value']
        result = {'configOptions':options}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#).unwrap();
    let mut spec = silent_bridge_spec(mj_core::activity::StallPolicy {
        silence: None,
        tool_call: None,
    });
    spec.command = "python3".into();
    spec.args = vec![
        script.to_string_lossy().into_owned(),
        temp.path().to_string_lossy().into_owned(),
        fail_replacement.to_string(),
    ];
    spec.cwd = temp.path().to_path_buf();
    spec.harness = harness;
    let (request_tx, request_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let runtime = tokio::spawn(run(spec, request_rx, event_tx));
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::SessionConfigured { .. })
    })
    .await;
    request_tx
        .send(CommandRequest::SetSessionMode {
            request_id: "select-plan".into(),
            mode_id: "plan".into(),
        })
        .await
        .unwrap();
    wait_for_runtime_event(&mut event_rx, |event| matches!(event, RuntimeEvent::SessionModeApplied { request_id, .. } if request_id == "select-plan")).await;
    request_tx
        .send(CommandRequest::ClearContext {
            request_id: "clear-request".into(),
        })
        .await
        .unwrap();
    if fail_replacement {
        wait_for_runtime_event(&mut event_rx, |event| matches!(event, RuntimeEvent::CommandRejected { request_id, message } if request_id == "clear-request" && message.contains("replacement refused"))).await;
        wait_for_runtime_event(&mut event_rx, |event| matches!(event, RuntimeEvent::SessionStarted { native_session_id, resumed: true, .. } if native_session_id == "original")).await;
    } else {
        wait_for_runtime_event(&mut event_rx, |event| matches!(event, RuntimeEvent::ContextCleared { request_id, native_session_id, .. } if request_id == "clear-request" && native_session_id == "replacement")).await;
    }
    wait_for_runtime_event(&mut event_rx, |event| {
        matches!(event, RuntimeEvent::SessionConfigured { .. })
    })
    .await;
    drop(request_tx);
    // Drain the bounded event channel while shutdown completes.
    let drain = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    tokio::time::timeout(Duration::from_secs(20), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drain.await.unwrap();
    let requests = std::fs::read_to_string(temp.path().join("requests.jsonl")).unwrap();
    let methods: Vec<String> = requests
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(!methods.iter().any(|method| method == "session/prompt"));
    {
        let requests: Vec<serde_json::Value> = requests
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            requests
                .iter()
                .any(|request| request["method"] == "session/set_config_option"
                    && request["params"]["configId"] == "fast"
                    && request["params"]["value"] == "true")
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/set_mode"
                    && request["params"]["modeId"] == "plan")
                .count(),
            2
        );
    }
    assert_eq!(
        methods
            .iter()
            .filter(|method| method.as_str() == "session/new")
            .count(),
        2
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| method.as_str() == "session/load")
            .count(),
        usize::from(fail_replacement)
    );
}

/// Opt-in authenticated adapter check; uses a disposable project and never opens
/// the controller database. Supply MJ_CLEAR_ADAPTER and MJ_CLEAR_HARNESS.
#[tokio::test]
#[ignore = "requires an authenticated ACP adapter and makes live provider requests"]
async fn live_adapter_compacts_and_replaces_context() {
    let temp = tempfile::tempdir().unwrap();
    let mut spec = silent_bridge_spec(mj_core::activity::StallPolicy {
        silence: None,
        tool_call: None,
    });
    spec.command = std::env::var("MJ_CLEAR_ADAPTER").unwrap().into();
    spec.harness = match std::env::var("MJ_CLEAR_HARNESS").unwrap().as_str() {
        "codex" => HarnessKind::Codex,
        "claude" => HarnessKind::Claude,
        other => panic!("unsupported live harness: {other}"),
    };
    spec.cwd = temp.path().to_path_buf();
    spec.verdict = None;
    let (tx, rx) = mpsc::channel(4);
    let (events_tx, mut events) = mpsc::channel(256);
    let runtime = tokio::spawn(run(spec, rx, events_tx));
    let mut original = None;
    async fn next(events: &mut mpsc::Receiver<RuntimeEvent>) -> RuntimeEvent {
        tokio::time::timeout(Duration::from_secs(180), events.recv())
            .await
            .unwrap()
            .expect("adapter remains connected")
    }
    loop {
        match next(&mut events).await {
            RuntimeEvent::SessionStarted {
                native_session_id, ..
            } => original = Some(native_session_id),
            RuntimeEvent::SessionConfigured { .. } => break,
            _ => {}
        }
    }
    for (id, text) in [
        ("hello", "Reply with just hello. Do not use tools."),
        ("compact", "/compact"),
    ] {
        tx.send(CommandRequest::Prompt {
            request_id: id.into(),
            prompt: vec![ContentBlock::from(text)],
        })
        .await
        .unwrap();
        loop {
            match next(&mut events).await {
                RuntimeEvent::PromptFinished { request_id, .. } if request_id == id => break,
                RuntimeEvent::CommandRejected {
                    request_id,
                    message,
                } if request_id == id => panic!("{id}: {message}"),
                _ => {}
            }
        }
        eprintln!("live adapter: {id} completed");
    }
    tx.send(CommandRequest::ClearContext {
        request_id: "clear".into(),
    })
    .await
    .unwrap();
    loop {
        match next(&mut events).await {
            RuntimeEvent::ContextCleared {
                native_session_id, ..
            } => {
                assert_ne!(Some(native_session_id), original);
                eprintln!("live adapter: native identity replaced");
            }
            RuntimeEvent::SessionConfigured { .. } => break,
            RuntimeEvent::CommandRejected { message, .. } => panic!("clear: {message}"),
            _ => {}
        }
    }
    drop(tx);
    while events.recv().await.is_some() {}
    runtime.await.unwrap().unwrap();
}

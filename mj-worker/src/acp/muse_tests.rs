use super::*;
use agent_client_protocol::schema::v1::{ImageContent, TextContent};
use std::path::Path;

/// Exercise the actual client and native host without printing credentials or
/// provider transcripts in diagnostics.
pub(crate) async fn native_muse_turn(
    adapter: &Path,
    muse: &Path,
    home: &Path,
    cwd: &Path,
    resume: Option<String>,
    prompt: &str,
) -> (String, String) {
    let mut environment = BTreeMap::from([
        ("MUSE_CLI".into(), muse.to_string_lossy().into_owned()),
        (
            "MUSE_SERVE_ARGS".into(),
            "--disable-shell --disable-write".into(),
        ),
    ]);
    HarnessKind::Muse.configure_home_environment(home, &mut environment);
    let resuming = resume.is_some();
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_policy: mj_core::subagent::SubagentPolicy::Native,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: adapter.to_path_buf(),
        args: Vec::new(),
        environment,
        cwd: cwd.to_path_buf(),
        additional_directories: Vec::new(),
        project_memory: None,
        extra_mcp_servers: Vec::new(),
        resume_session: resume,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        initial_model: None,
        harness: HarnessKind::Muse,
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
    let (commands, receiver) = mpsc::channel(16);
    let (sender, mut events) = mpsc::channel(128);
    let task = tokio::spawn(run(spec, receiver, sender));
    let mut session_id = String::new();
    let mut reply = String::new();
    let stop_reason = loop {
        let event = tokio::time::timeout(Duration::from_secs(120), events.recv())
            .await
            .expect("native Muse timed out")
            .expect("native Muse stopped unexpectedly");
        match event {
            RuntimeEvent::SessionStarted {
                native_session_id,
                resumed,
                ..
            } => {
                assert_eq!(resumed, resuming);
                eprintln!("Native Muse session ready (resumed={resumed})");
                session_id = native_session_id;
                commands
                    .send(CommandRequest::Prompt {
                        request_id: "native-smoke".into(),
                        prompt: vec![ContentBlock::Text(TextContent::new(prompt))],
                    })
                    .await
                    .unwrap();
            }
            RuntimeEvent::SessionUpdate { update } => {
                if update["sessionUpdate"] == "agent_message_chunk"
                    && let Some(text) = update["content"]["text"].as_str()
                {
                    reply.push_str(text);
                }
            }
            RuntimeEvent::PromptFinished { stop_reason, .. } => {
                break stop_reason;
            }
            RuntimeEvent::ElicitationRequested { request } => {
                let (resolved, received) = oneshot::channel();
                commands
                    .send(CommandRequest::ResolveElicitation {
                        elicitation_id: request.id,
                        response: ElicitationResponse::Cancel,
                        resolved,
                    })
                    .await
                    .unwrap();
                assert_eq!(received.await.unwrap(), Ok(()));
            }
            RuntimeEvent::Warning { message } if message.contains("ACP runtime failed") => {
                panic!(
                    "native Muse ACP runtime failed; check authentication and runtime configuration"
                );
            }
            _ => {}
        }
    };
    commands
        .send(CommandRequest::Close {
            request_id: "native-close".into(),
        })
        .await
        .unwrap();
    drop(commands);
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!session_id.is_empty());
    assert_eq!(stop_reason, "EndTurn", "native Muse turn failed");
    (session_id, reply)
}

async fn next(events: &mut mpsc::Receiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(15), events.recv())
        .await
        .expect("Muse event timed out")
        .expect("Muse runtime stopped unexpectedly")
}

/// What opening a session through the fake muse-acp produced.
struct Opened {
    outcome: Result<()>,
    configured: bool,
    warnings: Vec<String>,
    /// The MCP server names each `session/new` carried.
    received: Vec<Vec<String>>,
}

/// Open a session through a fake muse-acp whose host grants session MCP,
/// withholds it, or could not start. A session carries project memory.
async fn open_muse(host: &str) -> Opened {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("muse_acp.py");
    let log = root.path().join("mcp.jsonl");
    std::fs::write(
        &script,
        r#"
import json, os, sys
host, log = os.environ['MJ_FAKE_MUSE_HOST'], os.environ['MJ_FAKE_MUSE_LOG']
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize':
        # muse-acp advertises HTTP MCP exactly when its host granted sessionMcp.
        reply = {'result': {'protocolVersion': 1, 'agentCapabilities': {
            'mcpCapabilities': {'http': host == 'granted'}}}}
    elif method == 'session/new':
        with open(log, 'a') as out:
            out.write(json.dumps([s['name'] for s in params.get('mcpServers', [])]) + '\n')
        if host == 'failed':
            reply = {'error': {'code': -32000, 'message': 'Muse Code is not logged in'}}
        else:
            reply = {'result': {'sessionId': 'native'}}
    else:
        reply = {'result': {}}
    print(json.dumps({'jsonrpc': '2.0', 'id': ident, **reply}), flush=True)
"#,
    )
    .unwrap();
    let spec = LaunchSpec {
        bridge_spec_path: None,
        subagent_policy: mj_core::subagent::SubagentPolicy::Native,
        subagent_mcp_socket: None,
        clear_context_request: None,
        context_restore: None,
        goal_recovery: Default::default(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::from([
            ("MJ_FAKE_MUSE_HOST".into(), host.into()),
            (
                "MJ_FAKE_MUSE_LOG".into(),
                log.to_string_lossy().into_owned(),
            ),
        ]),
        cwd: root.path().to_path_buf(),
        additional_directories: Vec::new(),
        project_memory: Some(ProjectMemoryLaunchConfig {
            history_socket: None,
            project_key: "abc".into(),
            root: root.path().join("memory"),
            baseline_root: root.path().join(".hel-memory-baseline"),
            repository_roots: BTreeMap::new(),
            mcp_delivery: ProjectMemoryMcpDelivery::Acp,
        }),
        extra_mcp_servers: Vec::new(),
        resume_session: None,
        native_session_may_have_history: false,
        accepted_config: Default::default(),
        initial_model: None,
        harness: HarnessKind::Muse,
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
    let (commands, receiver) = mpsc::channel(16);
    let (sender, mut events) = mpsc::channel(128);
    let runtime = tokio::spawn(run(spec, receiver, sender));
    let mut configured = false;
    let mut warnings = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(15), events.recv())
        .await
        .expect("the fake adapter must make progress")
    {
        match event {
            RuntimeEvent::SessionConfigured { .. } => {
                configured = true;
                break;
            }
            RuntimeEvent::Warning { message } => warnings.push(message),
            _ => {}
        }
    }
    drop(commands);
    let outcome = tokio::time::timeout(Duration::from_secs(15), runtime)
        .await
        .expect("the runtime must stop")
        .unwrap();
    let received = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Opened {
        outcome,
        configured,
        warnings,
        received,
    }
}

// Hard-won: c5deb2a: Older Muse containers stopped working after upgrades when their adapter withheld MCP servers.
#[tokio::test]
async fn muse_sessions_receive_mcp_servers_and_say_so_when_the_host_withholds_them() {
    let opened = open_muse("granted").await;
    opened.outcome.unwrap();
    assert!(opened.configured);
    assert!(opened.warnings.is_empty(), "{:?}", opened.warnings);
    assert_eq!(opened.received, [["mj-memory"]]);

    // A container keeps the adapter it was created with. Its session keeps
    // working without the tools, and the person is told why.
    let opened = open_muse("withheld").await;
    opened.outcome.unwrap();
    assert!(opened.configured);
    assert_eq!(opened.received, [["mj-memory"]]);
    assert!(
        opened
            .warnings
            .iter()
            .any(|warning| warning.contains("does not accept MCP servers")),
        "{:?}",
        opened.warnings
    );
}

// Hard-won: c5deb2a: A Muse reviewer could run without the analyzer tools required to review safely.
#[tokio::test]
async fn a_muse_host_that_could_not_start_reports_its_own_diagnostic() {
    // A host that never started also withholds the grant; its diagnostic, not
    // the missing grant, is what the person has to act on.
    let opened = open_muse("failed").await;
    let error = format!("{:#}", opened.outcome.unwrap_err());
    assert!(error.contains("Muse Code is not logged in"), "{error}");
    assert!(!error.contains("MCP"), "{error}");
    assert!(!opened.configured);
}

#[tokio::test]
#[ignore = "requires MJ_MUSE_ACP_TEST_BINARY pointing to verified muse-acp 0.10.0"]
async fn real_muse_adapter_chat_selectors_images_permissions_questions_and_resume() {
    let adapter =
        PathBuf::from(std::env::var_os("MJ_MUSE_ACP_TEST_BINARY").expect("set adapter path"));
    let host = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/e2e/muse_host.py");
    use ExecutionPolicy::{ConfiguredApprovals, Unconstrained};
    for (scenario, choice, resume, policy) in [
        ("chat", "", false, ConfiguredApprovals),
        ("chat", "", true, ConfiguredApprovals),
        ("chat", "", false, Unconstrained),
        ("permission", "allow", false, ConfiguredApprovals),
        ("permission", "deny", false, ConfiguredApprovals),
        ("question", "accept", false, ConfiguredApprovals),
        ("question", "cancel", false, ConfiguredApprovals),
        ("quiet", "", false, ConfiguredApprovals),
    ] {
        eprintln!(
            "Muse scenario: {scenario}, choice: {choice}, resume: {resume}, policy: {policy:?}"
        );
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("host.jsonl");
        let environment = BTreeMap::from([
            ("MUSE_CLI".into(), host.to_string_lossy().into_owned()),
            (
                "MJ_MUSE_TEST_LOG".into(),
                log.to_string_lossy().into_owned(),
            ),
            ("MJ_MUSE_TEST_SCENARIO".into(), scenario.into()),
            // The verdict the fake auto-review host returns.
            (
                "MJ_MUSE_TEST_REVIEW".into(),
                if choice == "deny" { "deny" } else { "allow" }.into(),
            ),
        ]);
        let spec = LaunchSpec {
            bridge_spec_path: None,
            subagent_policy: mj_core::subagent::SubagentPolicy::Native,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: adapter.clone(),
            args: Vec::new(),
            environment,
            cwd: temp.path().to_path_buf(),
            additional_directories: Vec::new(),
            project_memory: None,
            extra_mcp_servers: Vec::new(),
            resume_session: resume.then(|| "01991be0-0000-7000-8000-000000000001".into()),
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            initial_model: None,
            harness: HarnessKind::Muse,
            execution_policy: policy,
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
        let (commands, receiver) = mpsc::channel(16);
        let (sender, mut events) = mpsc::channel(128);
        let task = tokio::spawn(run(spec, receiver, sender));
        loop {
            match next(&mut events).await {
                RuntimeEvent::SessionStarted { resumed, .. } => {
                    assert_eq!(resumed, resume);
                    break;
                }
                RuntimeEvent::SessionUpdate { update } => {
                    assert!(!update.to_string().contains("must not replay"))
                }
                RuntimeEvent::Warning { message } => panic!("Muse startup failed: {message}"),
                _ => {}
            }
        }
        commands
            .send(CommandRequest::SetConfig {
                request_id: "effort".into(),
                key: "reasoning_effort".into(),
                value: "high".into(),
            })
            .await
            .unwrap();
        loop {
            if let RuntimeEvent::ConfigApplied { key, value, .. } = next(&mut events).await {
                assert_eq!((key.as_str(), value.as_str()), ("reasoning_effort", "high"));
                break;
            }
        }
        commands
            .send(CommandRequest::Prompt {
                request_id: "prompt".into(),
                prompt: vec![
                    ContentBlock::Text(TextContent::new("test Muse")),
                    ContentBlock::Image(ImageContent::new("QUJD".repeat(32768), "image/png")),
                ],
            })
            .await
            .unwrap();
        if scenario == "quiet" {
            // Wait for host admission, so cancellation tests an active turn.
            for _ in 0..200 {
                if std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .contains("turn/start")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            commands
                .send(CommandRequest::Cancel {
                    request_id: "cancel".into(),
                    steering_prompt: None,
                })
                .await
                .unwrap();
        }
        let mut answered = false;
        let mut replied = false;
        loop {
            match next(&mut events).await {
                RuntimeEvent::SessionUpdate { update } => {
                    assert!(!update.to_string().contains("must not replay"));
                    replied |= update.to_string().contains("Muse test reply");
                }
                RuntimeEvent::ElicitationRequested { request } => {
                    // Guardian answers Muse permissions with auto-review.
                    assert_eq!(scenario, "question", "{request:?}");
                    let field = &request.fields[0];
                    let response = if choice == "accept" {
                        ElicitationResponse::Accept {
                            content: BTreeMap::from([(
                                field.id.clone(),
                                ElicitationValue::String("First".into()),
                            )]),
                        }
                    } else {
                        ElicitationResponse::Cancel
                    };
                    let (resolved, result) = oneshot::channel();
                    commands
                        .send(CommandRequest::ResolveElicitation {
                            elicitation_id: request.id,
                            response,
                            resolved,
                        })
                        .await
                        .unwrap();
                    assert_eq!(result.await.unwrap(), Ok(()));
                    answered = true;
                }
                RuntimeEvent::PromptFinished { stop_reason, .. } => {
                    assert_ne!(stop_reason, "error", "scenario {scenario}");
                    break;
                }
                RuntimeEvent::Warning { message } if message.contains("ACP runtime failed") => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
        if scenario == "chat" {
            assert!(replied);
        }
        if scenario == "question" {
            assert!(answered, "scenario {scenario} did not request user input");
        }
        commands
            .send(CommandRequest::Close {
                request_id: "close".into(),
            })
            .await
            .unwrap();
        drop(commands);
        tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let trace = std::fs::read_to_string(log).unwrap();
        assert!(trace.contains("QUJDQUJD"));
        // muse-acp 0.10 keeps the approval policy on `approval_mode`, apart
        // from its session modes.
        let approval_modes: Vec<serde_json::Value> = trace
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|message| message["method"] == "session/setApprovalMode")
            .map(|message| message["params"]["mode"].clone())
            .collect();
        let approval_mode = match policy {
            Unconstrained => "allowAll",
            ConfiguredApprovals => "promptUnmatched",
        };
        assert_eq!(approval_modes, [serde_json::json!(approval_mode)]);
        assert!(trace.contains("reasoningEffort"));
        if scenario == "question" {
            let method = if choice == "accept" {
                "userInput/answer"
            } else {
                "userInput/cancel"
            };
            let response: serde_json::Value = trace
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|message| message["method"] == method)
                .expect("Muse received the question response");
            if choice == "accept" {
                assert_eq!(response["params"]["answers"][0]["selectedLabel"], "First");
            }
        }
        if scenario == "permission" {
            let decision: serde_json::Value = trace
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|message| message["method"] == "approval/decide")
                .expect("Muse received permission decision");
            assert!(
                decision["params"].to_string().contains(choice),
                "{decision}"
            );
        }
    }
}

use super::*;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn spec(command: PathBuf, environment: BTreeMap<String, String>, cwd: PathBuf) -> LaunchSpec {
    LaunchSpec {
        goal_recovery: Default::default(),
        command,
        args: vec![],
        environment,
        cwd,
        additional_directories: vec![],
        extra_mcp_servers: vec![],
        project_memory: None,
        resume_session: None,
        accepted_config: Default::default(),
        harness: HarnessKind::Grok,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: StepClock::default(),
    }
}

fn report() -> serde_json::Value {
    serde_json::json!({"inputTokens":100,"outputTokens":30,"totalTokens":130,"cachedReadTokens":40,
        "cacheCreationTokens":10,"reasoningTokens":20,"modelCalls":2,"apiDurationMs":1700,"costUsdTicks":88767200,
        "modelUsage":{"grok-4.6-build":{"inputTokens":100,"outputTokens":30,"costUsdTicks":88767200}}})
}

#[tokio::test]
async fn grok_usage_crosses_acp_with_metadata_duplicates_and_late_notifications() {
    let (client, bridge) = tokio::io::duplex(64 * 1024);
    let bridge = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(bridge);
        let mut lines = BufReader::new(read).lines();
        let mut prompts = 0;
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = request["id"].clone();
            let result = match request["method"].as_str().unwrap_or("") {
                "initialize" => serde_json::json!({"protocolVersion":1}),
                "session/new" => serde_json::json!({"sessionId":"grok-test"}),
                "session/prompt" => {
                    prompts += 1;
                    let native_id = format!("native-{prompts}");
                    let notification = serde_json::json!({"jsonrpc":"2.0","method":"_x.ai/session/update","params":{
                        "sessionId":"grok-test","update":{"sessionUpdate":"turn_completed","prompt_id":native_id,"usage":report(),"elapsed_ms":2000}
                    }});
                    if prompts == 1 {
                        let chunk = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"grok-test","update":{
                            "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x".repeat(80*1024)}
                        }}});
                        write
                            .write_all(format!("{chunk}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                    if prompts == 2 {
                        for _ in 0..2 {
                            write
                                .write_all(format!("{notification}\n").as_bytes())
                                .await
                                .unwrap();
                        }
                    }
                    if prompts == 3 {
                        let response = serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn","_meta":{"promptId":native_id}}});
                        write
                            .write_all(format!("{response}\n").as_bytes())
                            .await
                            .unwrap();
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        write
                            .write_all(format!("{notification}\n").as_bytes())
                            .await
                            .unwrap();
                        continue;
                    }
                    serde_json::json!({"stopReason":"end_turn","usage":{"inputTokens":100,"outputTokens":30,"totalTokens":130},"_meta":{"promptId":native_id,"usage":report()}})
                }
                _ => continue,
            };
            let response = serde_json::json!({"jsonrpc":"2.0","id":id,"result":result});
            if write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
        prompts
    });
    let (read, write) = tokio::io::split(client);
    let transport = ByteStreams::new(write.compat_write(), read.compat());
    let (requests, mut request_rx) = mpsc::channel(4);
    let (events, mut event_rx) = mpsc::channel(16);
    let driver = tokio::spawn(async move {
        drive(
            transport,
            spec(
                "fake".into(),
                BTreeMap::new(),
                std::env::current_dir().unwrap(),
            ),
            &mut request_rx,
            events,
            Arc::new(Mutex::new(None)),
            false,
        )
        .await
    });
    for index in 1..=3 {
        requests
            .send(CommandRequest::Prompt {
                request_id: format!("prompt-{index}"),
                prompt: vec![ContentBlock::Text(TextContent::new("test"))],
            })
            .await
            .unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                RuntimeEvent::Warning { message } if message.contains("usage report") => {
                    panic!("{message}")
                }
                RuntimeEvent::PromptFinished {
                    usage, stop_reason, ..
                } => {
                    assert_eq!(stop_reason, "EndTurn");
                    let usage =
                        usage.expect("Grok full-turn usage must survive the actual ACP connection");
                    assert_eq!(usage.scope, mj_core::usage::UsageScope::Turn);
                    assert_eq!(usage.total_tokens, 130);
                    assert_eq!(usage.cached_read_tokens, Some(40));
                    let details = usage.provider_details.unwrap();
                    assert_eq!(details.cost.unwrap().usd, "0.0088767200");
                    if index > 1 {
                        assert_eq!(details.elapsed_ms, Some(2000));
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    drop(requests);
    tokio::time::timeout(Duration::from_secs(10), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bridge.await.unwrap(), 3);
}

#[tokio::test]
#[ignore = "requires GROK_VALIDATION_PROGRAM and GROK_VALIDATION_AUTH_FILE; runs one minimal live turn"]
async fn grok_live_usage_reaches_runtime_completion() {
    let program = PathBuf::from(
        std::env::var_os("GROK_VALIDATION_PROGRAM").expect("explicit Grok executable"),
    );
    let auth = PathBuf::from(
        std::env::var_os("GROK_VALIDATION_AUTH_FILE").expect("explicit authentication source"),
    );
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/grok-acp-validation");
    std::fs::create_dir_all(&parent).unwrap();
    let temp = tempfile::tempdir_in(parent).unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    std::fs::copy(auth, home.join("auth.json")).unwrap();
    let environment = BTreeMap::from([("GROK_HOME".into(), home.to_string_lossy().into_owned())]);
    let mut spec = spec(program, environment, cwd);
    spec.args = vec!["agent".into(), "--no-leader".into(), "stdio".into()];
    let (requests, request_rx) = mpsc::channel(4);
    let (events, mut event_rx) = mpsc::channel(64);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let driver = tokio::spawn(run_with_shutdown(spec, request_rx, events, stop));
    requests
        .send(CommandRequest::Prompt {
            request_id: "live-usage".into(),
            prompt: vec![ContentBlock::Text(TextContent::new(
                "Reply with exactly OK. Do not use tools.",
            ))],
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        while let Some(event) = event_rx.recv().await {
            match event {
                RuntimeEvent::PromptFinished {
                    usage, stop_reason, ..
                } => return (usage, stop_reason),
                RuntimeEvent::Warning { message } => eprintln!("{message}"),
                RuntimeEvent::Stopped => panic!("runtime stopped before the live prompt completed"),
                _ => {}
            }
        }
        panic!("runtime closed before completing")
    })
    .await;
    shutdown.cancel();
    // Keep draining while cleanup sends its final events.
    let drain = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
    tokio::time::timeout(Duration::from_secs(30), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drain.await.unwrap();
    let (usage, stop_reason) = result.unwrap();
    assert_eq!(stop_reason, "EndTurn");
    let usage = usage.expect("live Grok response must include consumption");
    assert_eq!(usage.scope, mj_core::usage::UsageScope::Turn);
    assert_eq!(usage.total_tokens, usage.input_tokens + usage.output_tokens);
    assert!(usage.total_tokens > 0);
    assert!(
        !usage
            .provider_details
            .as_ref()
            .unwrap()
            .model_usage
            .is_empty()
    );
    eprintln!(
        "live Grok normalized usage: {}",
        serde_json::to_string(&usage).unwrap()
    );
}

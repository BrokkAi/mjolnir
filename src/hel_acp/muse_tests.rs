use super::*;
use agent_client_protocol::schema::v1::{ImageContent, TextContent};

async fn next(events: &mut mpsc::Receiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(15), events.recv())
        .await
        .expect("Muse event timed out")
        .expect("Muse runtime stopped unexpectedly")
}

#[tokio::test]
#[ignore = "requires MJ_MUSE_ACP_TEST_BINARY pointing to verified muse-acp 0.2.4"]
async fn real_muse_adapter_chat_selectors_images_permissions_questions_and_resume() {
    let adapter =
        PathBuf::from(std::env::var_os("MJ_MUSE_ACP_TEST_BINARY").expect("set adapter path"));
    let host = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/muse_host.py");
    for (scenario, choice, resume) in [
        ("chat", "", false),
        ("chat", "", true),
        ("permission", "allow", false),
        ("permission", "deny", false),
        ("question", "accept", false),
        ("question", "cancel", false),
        ("quiet", "", false),
    ] {
        eprintln!("Muse scenario: {scenario}, choice: {choice}, resume: {resume}");
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("host.jsonl");
        let environment = BTreeMap::from([
            ("MUSE_CLI".into(), host.to_string_lossy().into_owned()),
            (
                "MJ_MUSE_TEST_LOG".into(),
                log.to_string_lossy().into_owned(),
            ),
            ("MJ_MUSE_TEST_SCENARIO".into(), scenario.into()),
        ]);
        let spec = LaunchSpec {
            command: adapter.clone(),
            args: Vec::new(),
            environment,
            cwd: temp.path().to_path_buf(),
            additional_directories: Vec::new(),
            project_memory: None,
            extra_mcp_servers: Vec::new(),
            resume_session: resume.then(|| "01991be0-0000-7000-8000-000000000001".into()),
            accepted_config: Default::default(),
            harness: HarnessKind::Muse,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: AcpActivityClock::default(),
            step_clock: StepClock::default(),
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
                    let field = &request.fields[0];
                    let response = if scenario == "permission" {
                        ElicitationResponse::Accept {
                            content: BTreeMap::from([(
                                "choice".into(),
                                ElicitationValue::String(choice.into()),
                            )]),
                        }
                    } else {
                        assert_eq!(scenario, "question");
                        if choice == "accept" {
                            ElicitationResponse::Accept {
                                content: BTreeMap::from([(
                                    field.id.clone(),
                                    ElicitationValue::String("First".into()),
                                )]),
                            }
                        } else {
                            ElicitationResponse::Cancel
                        }
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
        if matches!(scenario, "permission" | "question") {
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

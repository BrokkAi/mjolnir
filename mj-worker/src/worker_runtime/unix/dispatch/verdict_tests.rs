use super::*;
use crate::acp::verdict_client::{VerdictClient, VerdictSource};
use mj_core::activity::ActivityState;
use tokio::io::AsyncReadExt;
use tracing::instrument::WithSubscriber;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

#[derive(Clone, Copy)]
enum WhileClassifying {
    Wait,
    NewPrompt,
    Stop,
    KeepCurrent,
}

async fn completed_turn_verdict(action: WhileClassifying) {
    completed_turn_with_background(action, "background_work", 0).await;
}

async fn completed_turn_with_background(
    action: WhileClassifying,
    choice: &'static str,
    tasks: usize,
) {
    completed_turn_response(action, choice, tasks, 0.95, 200).await;
}

async fn completed_turn_response(
    action: WhileClassifying,
    choice: &'static str,
    tasks: usize,
    confidence: f32,
    status: u16,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requested_tx, requested_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["state"]["phase"], "replied");
        assert_eq!(
            request["state"]["assistant_text_tail"],
            "Waiting for my agent to finish."
        );
        assert_eq!(request["state"]["background_commands"], tasks);
        requested_tx.send(()).unwrap();
        if release_rx.await.is_err() {
            return;
        }
        let body = serde_json::json!({"answers": {
            "waiting_on": {"choice":choice, "confidence":confidence},
            "asked_question": {"type":"noul", "noul":0.01}
        }})
        .to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "test").unwrap();
    durable.set_turn_verdict_harness(HarnessKind::Claude);
    durable.set_background_work_policy(crate::relay::BackgroundWorkPolicy::ClaudeTasks);
    durable
        .claude_background_tasks_changed(
            (0..tasks)
                .map(|id| crate::acp::ClaudeBackgroundTask {
                    task_id: id.to_string(),
                    description: "Old provisioning wait".into(),
                })
                .collect(),
        )
        .unwrap();
    durable.set_harness_turn_policy(crate::relay::HarnessTurnPolicy::ClaudeAdapter);
    let submit = |durable: &mut DurableRelay, id: &str| {
        let response = durable.handle(RelayRequestEnvelope {
            request_id: format!("submit-{id}"),
            protocol_version: crate::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Submit {
                command_id: id.into(),
                command: RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                        agent_client_protocol::schema::v1::TextContent::new("Do the work"),
                    )],
                },
            },
        });
        assert!(matches!(response.body, RelayResponseBody::Ok { .. }));
    };
    submit(&mut durable, "prompt-1");
    let relay = Arc::new(Mutex::new(durable));
    let (events_tx, events_rx) = mpsc::channel(16);
    let (wakes_tx, wakes_rx) = mpsc::channel(1);
    let (commands_tx, mut commands_rx) = mpsc::channel(4);
    let user_shells = crate::user_shell::UserShellRegistry::new(
        temp.path().to_owned(),
        BTreeMap::new(),
        events_tx.clone(),
    );
    let client = VerdictClient::new(VerdictSource::Direct {
        key: "fake-key".into(),
        endpoint,
    })
    .unwrap();
    let log_path = temp.path().join("worker.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(crate::DEFAULT_WORKER_LOG_FILTER)
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    let coordinator = tokio::spawn(
        run_relay_coordinator_with_verdict(
            relay.clone(),
            events_rx,
            wakes_rx,
            commands_tx,
            user_shells,
            None,
            Some(client),
        )
        .with_subscriber(subscriber),
    );
    events_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: vec![],
        })
        .await
        .unwrap();
    assert!(matches!(
        commands_rx.recv().await.unwrap(),
        CommandRequest::Prompt { .. }
    ));
    events_tx.send(RuntimeEvent::SessionUpdate { update: serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"Waiting for my agent to finish."}}) }).await.unwrap();
    events_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-1".into(),
            stop_reason: "end_turn".into(),
            usage: None,
            diagnostic: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), requested_rx)
        .await
        .unwrap()
        .unwrap();
    // Completion is published even while the classifier HTTP response is blocked.
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .active_prompt
            .is_none()
    );
    assert!(!matches!(
        relay.lock().unwrap().operational_state().activity_state(),
        ActivityState::Expecting { .. }
    ));
    if matches!(action, WhileClassifying::Stop) {
        events_tx.send(RuntimeEvent::Stopped).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), coordinator)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(release_tx);
        server.await.unwrap();
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("Jev classification requested"), "{log}");
        assert!(log.contains("cancelled"), "{log}");
        return;
    }
    if matches!(action, WhileClassifying::NewPrompt) {
        submit(&mut relay.lock().unwrap(), "prompt-2");
        wakes_tx.send(()).await.unwrap();
        assert!(
            matches!(commands_rx.recv().await.unwrap(), CommandRequest::Prompt { request_id, .. } if request_id == "prompt-2")
        );
    }
    release_tx.send(()).unwrap();
    server.await.unwrap();
    if matches!(action, WhileClassifying::Wait) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let state = relay.lock().unwrap().operational_state();
                if if choice == "background_work" {
                    state.expected_continuation.is_some()
                } else {
                    state.inferred_idle_since_ms.is_some()
                } {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let state = relay.lock().unwrap().operational_state();
        assert_eq!(state.background_commands.len(), tasks);
        if choice == "background_work" {
            assert_eq!(state.activity_state().is_working(), tasks > 0);
        } else {
            assert!(state.activity_state().is_idle());
        }
        if tasks > 0 {
            assert!(!state.is_quiet());
        }
        // Autonomous output starts a real harness turn and clears the guess.
        events_tx.send(RuntimeEvent::SessionUpdate { update: serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"The agent finished."}}) }).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if relay
                    .lock()
                    .unwrap()
                    .operational_state()
                    .harness_turn
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    } else if matches!(action, WhileClassifying::KeepCurrent) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !std::fs::read_to_string(&log_path)
                .unwrap()
                .contains("Jev retry scheduled")
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let state = relay.lock().unwrap().operational_state();
        assert!(matches!(
            state.activity_state(),
            ActivityState::Background { .. }
        ));
        assert_eq!(state.inferred_idle_since_ms, None);
    } else {
        // Give the completed HTTP task a chance to reach the generation guard.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    assert!(!matches!(
        relay.lock().unwrap().operational_state().activity_state(),
        ActivityState::Expecting { .. }
    ));
    events_tx.send(RuntimeEvent::Stopped).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), coordinator)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("Jev classification requested"), "{log}");
    assert!(log.contains("summary_bytes="), "{log}");
    assert!(
        !log.contains("Waiting for my agent to finish."),
        "payloads must not be logged: {log}"
    );
    assert!(log.contains(SESSION_ID), "{log}");
    assert!(!log.contains("fake-key"));
    if matches!(action, WhileClassifying::Wait) {
        assert!(log.contains("Jev classification received"), "{log}");
        assert!(log.contains("confidence=0.95"), "{log}");
        assert!(log.contains("applied"), "{log}");
    }
}

#[tokio::test]
async fn replied_verdict_publishes_without_delaying_completion_and_clears_on_output() {
    completed_turn_verdict(WhileClassifying::Wait).await;
}

#[tokio::test]
async fn replied_verdict_cannot_change_a_newer_prompt() {
    completed_turn_verdict(WhileClassifying::NewPrompt).await;
}

#[tokio::test]
async fn shutdown_cancels_pending_replied_verdict() {
    completed_turn_verdict(WhileClassifying::Stop).await;
}

#[tokio::test]
async fn replied_verdict_classifies_harness_background_tasks_and_logs_the_decision() {
    for choice in ["finished", "user", "background_work"] {
        completed_turn_with_background(WhileClassifying::Wait, choice, 4).await;
    }
}

#[tokio::test]
async fn replied_idle_verdict_cannot_override_a_new_prompt() {
    completed_turn_with_background(WhileClassifying::NewPrompt, "finished", 4).await;
}

#[tokio::test]
async fn uncertain_and_failed_replied_verdicts_preserve_activity_and_schedule_retry() {
    completed_turn_response(WhileClassifying::KeepCurrent, "finished", 4, 0.5, 200).await;
    completed_turn_response(WhileClassifying::KeepCurrent, "unclear", 4, 0.95, 200).await;
    completed_turn_response(WhileClassifying::KeepCurrent, "finished", 4, 0.95, 503).await;
}

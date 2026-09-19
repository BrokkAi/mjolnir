use super::*;
use crate::acp::verdict_client::{VerdictClient, VerdictSource};
use mj_core::activity::ActivityState;
use tokio::io::AsyncReadExt;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

#[derive(Clone, Copy)]
enum WhileClassifying {
    Wait,
    NewPrompt,
    Stop,
}

async fn completed_turn_verdict(action: WhileClassifying) {
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
        requested_tx.send(()).unwrap();
        if release_rx.await.is_err() {
            return;
        }
        let body = serde_json::json!({"answers": {
            "waiting_on": {"choice":"background_work", "confidence":0.95},
            "asked_question": {"type":"noul", "noul":0.01}
        }})
        .to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    let client = VerdictClient::new(VerdictSource {
        key: "fake-key".into(),
        endpoint,
    })
    .unwrap();
    let coordinator = tokio::spawn(run_relay_coordinator_with_verdict(
        relay.clone(),
        events_rx,
        wakes_rx,
        commands_tx,
        user_shells,
        None,
        Some(client),
    ));
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
                if matches!(
                    relay.lock().unwrap().operational_state().activity_state(),
                    ActivityState::Expecting { .. }
                ) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
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

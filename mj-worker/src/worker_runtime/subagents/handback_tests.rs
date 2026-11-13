use super::*;
use crate::acp::verdict_client::{VerdictClient, VerdictSource};
use crate::relay::{DurableRelay, RelayCell, test_support::*};
use mj_core::config::HarnessKind;
use mj_core::subagent::{SubagentKind, SubagentToolAction};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

fn report(id: &str) -> SubagentToolRequest {
    SubagentToolRequest {
        originating_command_id: None,
        request_id: id.into(),
        created_at_ms: 1,
        action: SubagentToolAction::Handback {
            message: "Implementation is ready; validation remains.".into(),
        },
    }
}

async fn call(endpoint: SubagentEndpoint, request: SubagentToolRequest) -> serde_json::Value {
    let (mut client, server) = UnixStream::pair().unwrap();
    let handler = tokio::spawn(serve_one(server, endpoint));
    client
        .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).await.unwrap();
    handler.await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn pending_handback_check_withholds_publication_and_repeat_is_forwarded_without_http() {
    let root = tempfile::tempdir().unwrap();
    let mut owner = DurableRelay::open(root.path(), SESSION, "test").unwrap();
    owner.set_turn_verdict_harness(HarnessKind::Codex);
    owner
        .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    submit_relay(
        &mut owner,
        "subagent-spawn-first",
        prompt("Implement and validate the requested change."),
    );
    owner.claim_pending_commands(true).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    owner.handback_verdict_client = Some(
        VerdictClient::new(VerdictSource::Hosted {
            endpoint: format!("http://{}/", listener.local_addr().unwrap()),
        })
        .unwrap(),
    );
    let relay = Arc::new(RelayCell::new(owner));
    let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
    endpoint.relay = Some(relay.clone());
    endpoint.subagent_kind = Some(SubagentKind::Worker);
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let http = tokio::spawn(async move {
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
        seen_tx
            .send(serde_json::from_slice::<serde_json::Value>(&body).unwrap())
            .unwrap();
        release_rx.await.unwrap();
        let response = serde_json::json!({"probabilities": {
            "unfinished":0.5,"needs_parent_input":0.1,"blocked":0.1,
            "permitted_stop":0.1,"finished":0.1,"unclear":0.1
        }})
        .to_string();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    // Excluded identities do not consume the ordinary child's single attempt.
    for kind in [
        None,
        Some(SubagentKind::Reviewer),
        Some(SubagentKind::Oracle),
    ] {
        let mut excluded = endpoint.clone();
        excluded.subagent_kind = kind;
        assert!(
            excluded
                .check_handback(&mut report("excluded"))
                .await
                .unwrap()
                .is_none()
        );
    }
    let first = tokio::spawn(call(endpoint.clone(), report("first")));
    let evidence = seen_rx.await.unwrap();
    println!("HANDBACK_WORKER_BODY={}", evidence);
    let retry = tokio::spawn(call(endpoint.clone(), report("first")));
    let retry_gate_refs = Arc::strong_count(&endpoint.handback_gate);
    loop {
        if Arc::strong_count(&endpoint.handback_gate) > retry_gate_refs {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(evidence.to_string().contains("subagent-spawn-first"));
    assert!(
        endpoint.state.lock().unwrap().requests.is_empty(),
        "daemon must not see the handback while HTTP waits"
    );
    // Both state owners remain available while classification is awaiting HTTP.
    let _state = relay.lock().unwrap().operational_state();
    // Hard-won: 2afc06c9a: a concurrent handback with another request ID was delivered while the first check was pending
    let different = tokio::spawn(call(endpoint.clone(), report("different")));
    let different_gate_refs = Arc::strong_count(&endpoint.handback_gate);
    loop {
        if Arc::strong_count(&endpoint.handback_gate) > different_gate_refs {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        endpoint.state.lock().unwrap().requests.is_empty(),
        "neither handback may publish while the first HTTP check waits"
    );
    release_tx.send(()).unwrap();
    let bounced = first.await.unwrap();
    assert_eq!(
        retry.await.unwrap(),
        bounced,
        "same-ID retry must await and reuse the first decision"
    );
    loop {
        if endpoint
            .state
            .lock()
            .unwrap()
            .requests
            .contains_key("different")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(http.await.unwrap(), ()); // Listener is gone: no second HTTP check.
    endpoint
        .complete(SubagentToolResult {
            request_id: "different".into(),
            completed_at_ms: 1,
            is_error: false,
            message: "Report delivered".into(),
        })
        .unwrap();
    assert_eq!(
        different.await.unwrap()["result"]["message"],
        "Report delivered"
    );
    assert_eq!(bounced["result"]["is_error"], false);
    assert!(
        bounced["result"]["message"]
            .as_str()
            .unwrap()
            .contains("Not delivered")
    );
    assert!(
        !endpoint
            .state
            .lock()
            .unwrap()
            .requests
            .contains_key("first")
    );
    let second = tokio::spawn(call(endpoint.clone(), report("second")));
    let mut changes = endpoint.generations();
    loop {
        if endpoint
            .state
            .lock()
            .unwrap()
            .requests
            .contains_key("second")
        {
            break;
        }
        changes.changed().await.unwrap();
    }
    endpoint
        .complete(SubagentToolResult {
            request_id: "second".into(),
            completed_at_ms: 1,
            is_error: false,
            message: "Report delivered".into(),
        })
        .unwrap();
    assert_eq!(
        second.await.unwrap()["result"]["message"],
        "Report delivered"
    );
}

// Hard-won: 8ad2f6a1d: a slow optional handback check delayed delivery to the parent.
#[tokio::test]
async fn slow_handback_glide_delivers_unchecked_and_consumes_check() {
    let root = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let mut owner = DurableRelay::open(root.path(), SESSION, "test").unwrap();
    owner.set_turn_verdict_harness(HarnessKind::Codex);
    owner
        .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    submit_relay(
        &mut owner,
        "subagent-spawn-first",
        prompt("Implement and validate."),
    );
    owner.claim_pending_commands(true).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    owner.handback_verdict_client = Some(
        VerdictClient::new(VerdictSource::Hosted {
            endpoint: format!("http://{}/", listener.local_addr().unwrap()),
        })
        .unwrap()
        .with_log(Some(
            mj_core::jev::DecisionLog::open(logs.path().into()).unwrap(),
        )),
    );
    let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
    endpoint.relay = Some(Arc::new(RelayCell::new(owner)));
    endpoint.subagent_kind = Some(SubagentKind::Worker);
    let http = tokio::spawn(async move {
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
        tokio::time::sleep(Duration::from_secs(7)).await;
        // The caller has already delivered unchecked; no response is needed.
    });
    let started = Instant::now();
    let first = tokio::spawn(call(endpoint.clone(), report("slow")));
    let mut changes = endpoint.generations();
    loop {
        if endpoint.state.lock().unwrap().requests.contains_key("slow") {
            break;
        }
        tokio::time::timeout(Duration::from_secs(7), changes.changed())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(7));
    endpoint
        .complete(SubagentToolResult {
            request_id: "slow".into(),
            completed_at_ms: 1,
            is_error: false,
            message: "Report delivered".into(),
        })
        .unwrap();
    assert_eq!(
        first.await.unwrap()["result"]["message"],
        "Report delivered"
    );
    let repeat_started = Instant::now();
    assert!(
        endpoint
            .check_handback(&mut report("repeat"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(repeat_started.elapsed() < Duration::from_secs(1));
    let page = mj_core::jev::read(logs.path(), SESSION, None).unwrap();
    let page = mj_core::jev::read(logs.path(), SESSION, Some(&page.decisions[0].id)).unwrap();
    let technical = page.decisions[0].technical.as_ref().unwrap();
    assert_eq!(technical["outcome"], "delivered");
    assert_eq!(technical["reason"], "timeout");
    http.await.unwrap();
}

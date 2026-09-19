//! Optional, bounded TypeSafe turn classification. Failures leave activity alone.
use anyhow::{Context, Result, ensure};
use mj_core::activity::verdict::{TurnEvidence, TurnVerdict, api_key, questions};
use std::time::Duration;

#[derive(Clone)]
pub struct VerdictSource {
    pub key: String,
    pub endpoint: String,
}

impl std::fmt::Debug for VerdictSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerdictSource")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) struct VerdictClient {
    source: VerdictSource,
    client: reqwest::Client,
}

impl VerdictClient {
    pub(crate) async fn resolve(source: Option<&VerdictSource>) -> Option<Self> {
        let source = source.cloned();
        match tokio::task::spawn_blocking(move || Self::resolve_blocking(source)).await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "turn classifier initialization task failed");
                None
            }
        }
    }

    fn resolve_blocking(source: Option<VerdictSource>) -> Option<Self> {
        let source = source.or_else(|| {
            api_key().map(|key| VerdictSource {
                key,
                endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            })
        })?;
        if source.key.trim().is_empty() {
            return None;
        }
        match Self::new(source) {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "turn classifier unavailable");
                None
            }
        }
    }

    pub(crate) fn new(source: VerdictSource) -> Result<Self> {
        // Library callers may not pass through the standalone worker entry point.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("create turn classifier HTTP client")?;
        Ok(Self { source, client })
    }

    pub(crate) async fn ask(&self, evidence: &TurnEvidence) -> Result<TurnVerdict> {
        const MAX_RESPONSE_BYTES: usize = 64 * 1024;
        let mut response = self.client.post(&self.source.endpoint)
            .bearer_auth(&self.source.key)
            .json(&serde_json::json!({"model":"jev-latest", "state":evidence, "questions":questions()}))
            .send().await.context("request turn verdict")?
            .error_for_status().context("turn verdict HTTP status")?;
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_RESPONSE_BYTES as u64),
            "turn verdict response exceeds byte limit"
        );
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.context("read turn verdict")? {
            ensure!(
                body.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
                "turn verdict response exceeds byte limit"
            );
            body.extend_from_slice(&chunk);
        }
        TurnVerdict::parse(&serde_json::from_slice(&body).context("decode turn verdict JSON")?)
    }
}

/// Lives beside the prompt future, so cancellation and shutdown can always win
/// while HTTP is pending. Dropping the turn drops the request and its schedule.
pub(super) async fn await_input_verdict(spec: &super::LaunchSpec, client: &VerdictClient) {
    await_input_verdict_with_cadence(spec, client, mj_core::activity::SILENCE_WORTH_REPORTING)
        .await;
}

async fn await_input_verdict_with_cadence(
    spec: &super::LaunchSpec,
    client: &VerdictClient,
    first: Duration,
) {
    use mj_core::activity::verdict::{Decision, TurnPhase, decide};
    let mut observed = spec.acp_activity.last_at_ms();
    let mut gap = first;
    let mut next_silence = first;
    let mut warned = false;
    loop {
        let facts = super::turn_stall_facts(spec);
        if observed != facts.last_acp_activity_at_ms {
            observed = facts.last_acp_activity_at_ms;
            gap = first;
            next_silence = first;
        }
        let now = mj_core::clock::epoch_millis();
        let silent =
            Duration::from_millis(now.saturating_sub(observed.unwrap_or(now)).max(0) as u64);
        if silent < next_silence {
            tokio::time::sleep((next_silence - silent).min(Duration::from_secs(1))).await;
            continue;
        }
        let generation = spec.turn_context.generation();
        let evidence = spec
            .turn_context
            .evidence(spec.harness, TurnPhase::Running, &facts, now);
        let answer = client.ask(&evidence).await;
        if observed != spec.acp_activity.last_at_ms()
            || generation != spec.turn_context.generation()
        {
            continue;
        }
        match answer {
            Ok(verdict) if decide(TurnPhase::Running, &verdict) == Decision::AwaitingInput => {
                return;
            }
            Ok(_) => {}
            Err(error) => {
                if !warned {
                    tracing::warn!(%error, "turn classification failed; retaining current activity");
                    warned = true;
                }
            }
        }
        gap = (gap * 2).min(Duration::from_secs(300));
        next_silence = silent + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::activity::verdict::{TurnContext, TurnPhase, WaitingOn};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    async fn server(body: String) -> (VerdictClient, tokio::task::JoinHandle<serde_json::Value>) {
        delayed_server(body, Duration::ZERO).await
    }

    async fn delayed_server(
        body: String,
        delay: Duration,
    ) -> (VerdictClient, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = 0;
            let mut authorized = false;
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                authorized |= line.trim() == "authorization: Bearer test-key";
            }
            assert!(authorized);
            let mut request = vec![0; length];
            socket.read_exact(&mut request).await.unwrap();
            tokio::time::sleep(delay).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            serde_json::from_slice(&request).unwrap()
        });
        (
            VerdictClient::new(VerdictSource {
                key: "test-key".into(),
                endpoint,
            })
            .unwrap(),
            task,
        )
    }

    fn evidence() -> TurnEvidence {
        let context = TurnContext::default();
        context.reset("Implement the requested feature");
        context.evidence(
            mj_core::config::HarnessKind::Claude,
            TurnPhase::Running,
            &Default::default(),
            0,
        )
    }

    #[tokio::test]
    async fn classifier_sends_bounded_evidence_and_parses_typed_answers() {
        let (client, server) = server(
            serde_json::json!({"answers": {
                "waiting_on":{"type":"choice","choice":"user","confidence":0.95},
                "asked_question":{"type":"noul","noul":0.96}
            }})
            .to_string(),
        )
        .await;
        let answer = client.ask(&evidence()).await.unwrap();
        assert_eq!(answer.waiting_on, WaitingOn::User);
        let request = server.await.unwrap();
        assert_eq!(request["model"], "jev-latest");
        assert_eq!(request["state"]["phase"], "running");
        assert_eq!(request["questions"]["asked_question"]["type"], "noul");
    }

    #[tokio::test]
    async fn oversized_classifier_body_is_rejected() {
        let (client, server) = server(" ".repeat(128 * 1024)).await;
        let error = client.ask(&evidence()).await.unwrap_err();
        assert!(error.to_string().contains("exceeds byte limit"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unreachable_classifier_is_an_error_without_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let client = VerdictClient::new(VerdictSource {
            key: "test-key".into(),
            endpoint,
        })
        .unwrap();
        assert!(client.ask(&evidence()).await.is_err());
    }

    fn response(choice: &str) -> String {
        serde_json::json!({"answers": {
            "waiting_on":{"type":"choice","choice":choice,"confidence":0.95},
            "asked_question":{"type":"noul","noul":0.96}
        }})
        .to_string()
    }

    #[tokio::test]
    async fn a_running_turn_ends_only_for_a_confident_user_handoff() {
        for choice in ["user", "background_work", "unclear"] {
            let (client, server) = server(response(choice)).await;
            let spec = super::super::tests::silent_bridge_spec(mj_core::activity::StallPolicy {
                silence: None,
                tool_call: None,
            });
            spec.acp_activity.mark();
            let result = tokio::time::timeout(
                Duration::from_millis(150),
                await_input_verdict_with_cadence(&spec, &client, Duration::from_millis(5)),
            )
            .await;
            assert_eq!(result.is_ok(), choice == "user");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn renewed_activity_discards_an_in_flight_user_verdict() {
        let (client, server) = delayed_server(response("user"), Duration::from_millis(100)).await;
        let spec = super::super::tests::silent_bridge_spec(mj_core::activity::StallPolicy {
            silence: None,
            tool_call: None,
        });
        spec.acp_activity.mark();
        let activity = spec.acp_activity.clone();
        let update = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            activity.mark();
        });
        assert!(
            tokio::time::timeout(
                Duration::from_millis(250),
                await_input_verdict_with_cadence(&spec, &client, Duration::from_millis(5))
            )
            .await
            .is_err()
        );
        update.await.unwrap();
        server.await.unwrap();
    }
}

//! Optional, bounded TypeSafe turn classification. Failures leave activity alone.
use anyhow::{Context, Result, ensure};
use mj_core::activity::verdict::{TurnEvidence, TurnVerdict, api_key, questions};
use std::time::Duration;

const HOSTED_VERDICT_ENDPOINT: &str =
    "https://mj-jev-proxy.eng-admin-a63.workers.dev/v4/turn-verdict";
const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

#[derive(Clone)]
pub enum VerdictSource {
    /// An explicit blank key disables classification for isolated tests.
    Direct {
        key: String,
        endpoint: String,
    },
    Hosted {
        endpoint: String,
    },
}

impl VerdictSource {
    fn for_key(key: Option<String>) -> Self {
        match key {
            Some(key) => Self::Direct {
                key,
                endpoint: TYPESAFE_ENDPOINT.into(),
            },
            None => Self::Hosted {
                endpoint: HOSTED_VERDICT_ENDPOINT.into(),
            },
        }
    }
}

impl std::fmt::Debug for VerdictSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, endpoint) = match self {
            Self::Direct { endpoint, .. } => ("Direct", endpoint),
            Self::Hosted { endpoint } => ("Hosted", endpoint),
        };
        f.debug_struct(name)
            .field("endpoint", endpoint)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) struct VerdictClient {
    source: VerdictSource,
    client: reqwest::Client,
    log: Option<mj_core::jev::DecisionLog>,
}

/// Kept alive through application so cancellation has a terminal log event too.
pub(crate) struct VerdictAttempt {
    pub(crate) diagnostic: Option<mj_core::jev::Attempt>,
    decision: Option<mj_core::activity::verdict::Decision>,
    uncertain: bool,
    id: u64,
    session: String,
    generation: u64,
    phase: mj_core::activity::verdict::TurnPhase,
    started: std::time::Instant,
    finished: bool,
    dispatch: tracing::Dispatch,
}

impl VerdictAttempt {
    pub(crate) fn finish(&mut self, outcome: &str, reason: &str) {
        tracing::dispatcher::with_default(&self.dispatch, || {
            tracing::info!(target: "mj_jev", request_id = self.id, session = %self.session,
                generation = self.generation, phase = ?self.phase,
                elapsed_ms = self.started.elapsed().as_millis() as u64, outcome, reason,
                "Jev decision outcome");
        });
        if let Some(diagnostic) = &self.diagnostic {
            let (status, action) = match (outcome, reason) {
                (_, "request_failed") => (
                    "failed",
                    "Jev request failed; mj kept the runtime status.".to_owned(),
                ),
                ("discarded", _) => (
                    "stale",
                    "New activity superseded this assessment; mj kept the runtime status."
                        .to_owned(),
                ),
                ("applied", _) => (
                    "applied",
                    if reason == "server_retry_armed" {
                        "Mj scheduled a retry for a transient provider failure."
                    } else {
                    match self.decision {
                        Some(mj_core::activity::verdict::Decision::InferIdle) => {
                            "Mj marked the session ready."
                        }
                        Some(mj_core::activity::verdict::Decision::ExpectContinuation) => {
                            "Mj is expecting the agent to follow up."
                        }
                        _ => "Mj marked the session as awaiting input.",
                    }
                    }
                    .to_owned(),
                ),
                ("unchanged", "keep_current") if self.uncertain => (
                    "uncertain",
                    "The independent assessments did not meet the action thresholds; mj kept the runtime status.".into(),
                ),
                ("unchanged", "keep_current") => (
                    "unchanged",
                    "This answer does not change the status in this phase.".into(),
                ),
                ("cancelled", _) => (
                    "cancelled",
                    "Check cancelled before an assessment was applied.".into(),
                ),
                _ => (
                    outcome,
                    format!("Mj kept the runtime status: {}.", reason.replace('_', " ")),
                ),
            };
            diagnostic.update(None, serde_json::json!({
                "outcome": outcome, "reason": reason,
                "applied_decision": if outcome == "applied" { self.decision.map(|d| format!("{d:?}")) } else { None }
            }));
            diagnostic.finish(status, &action);
        }
        self.finished = true;
    }
}

impl Drop for VerdictAttempt {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("cancelled", "request_or_owner_dropped");
        }
    }
}

impl VerdictClient {
    pub(crate) async fn resolve(source: Option<&VerdictSource>) -> Option<Self> {
        let source = source.cloned();
        match tokio::task::spawn_blocking(move || Self::resolve_blocking(source)).await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(target: "mj_jev", %error, "turn classifier initialization task failed");
                None
            }
        }
    }

    fn resolve_blocking(source: Option<VerdictSource>) -> Option<Self> {
        Self::resolve_with(source, mj_core::jev::disabled_by_environment())
    }

    /// `jev_disabled` is `[jev] enabled = false` as the launch passed it.
    /// The switch covers the default source only: an explicit source is a
    /// test's own endpoint.
    fn resolve_with(source: Option<VerdictSource>, jev_disabled: bool) -> Option<Self> {
        if source.is_none() && jev_disabled {
            tracing::info!(target: "mj_jev", outcome = "disabled", reason = "jev_switch_off", "Jev classifier disabled");
            return None;
        }
        let source = source.unwrap_or_else(|| VerdictSource::for_key(api_key()));
        if matches!(&source, VerdictSource::Direct { key, .. } if key.trim().is_empty()) {
            tracing::info!(target: "mj_jev", outcome = "disabled", reason = "explicit_blank_key", "Jev classifier disabled");
            return None;
        }
        match Self::new(source) {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(target: "mj_jev", %error, "turn classifier unavailable");
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
        Ok(Self {
            source,
            client,
            log: None,
        })
    }

    pub(crate) fn with_log(mut self, log: Option<mj_core::jev::DecisionLog>) -> Self {
        self.log = log;
        self
    }

    fn request_body(&self, evidence: &TurnEvidence) -> serde_json::Value {
        match self.source {
            VerdictSource::Direct { .. } => {
                serde_json::json!({"model":"jev-latest", "state":evidence, "questions":questions()})
            }
            VerdictSource::Hosted { .. } => serde_json::json!(evidence),
        }
    }

    pub(crate) async fn ask_logged(
        &self,
        session: &str,
        generation: u64,
        evidence: &TurnEvidence,
    ) -> (VerdictAttempt, Result<TurnVerdict>) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
        let diagnostic = self.log.as_ref().map(|log| log.start(session, "activity",
            "Does the session need user input, expect more agent work, or appear finished?",
            "Current delivered user request and assistant conversation, plus live runtime facts. Transcript tool history is excluded; bounded summaries may omit older text."));
        if let Some(diagnostic) = &diagnostic {
            diagnostic.update(None, serde_json::json!({"request":self.request_body(evidence), "contract":"turn-verdict-v4", "questions":questions(), "model":"jev-latest", "confidence_threshold":mj_core::activity::verdict::ACT_CONFIDENCE, "no_input_threshold":mj_core::activity::verdict::NO_INPUT_CONFIDENCE, "server_retry_threshold":mj_core::activity::verdict::SERVER_RETRY_CONFIDENCE, "generation":generation, "source":if matches!(self.source, VerdictSource::Direct { .. }) { "direct" } else { "hosted" }}));
        }
        let mut attempt = VerdictAttempt {
            diagnostic,
            decision: None,
            uncertain: false,
            id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            session: session.into(),
            generation,
            phase: evidence.phase,
            started: std::time::Instant::now(),
            finished: false,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
        };
        let source = match self.source {
            VerdictSource::Direct { .. } => "direct",
            VerdictSource::Hosted { .. } => "hosted",
        };
        tracing::info!(target: "mj_jev", request_id = attempt.id, session, generation,
            phase = ?evidence.phase, harness = ?evidence.harness, source,
            summary_bytes = evidence.transcript_summary.len(),
            active_tools = evidence.tools_in_flight.len(),
            "Jev classification requested");
        let result = self.ask(evidence).await;
        if let Some(diagnostic) = &attempt.diagnostic {
            match &result {
                Ok(answer) => {
                    attempt.decision =
                        Some(mj_core::activity::verdict::decide(evidence.phase, answer));
                    attempt.uncertain = answer.needs_user_input
                        < mj_core::activity::verdict::ACT_CONFIDENCE
                        && (answer.needs_user_input
                            > mj_core::activity::verdict::NO_INPUT_CONFIDENCE
                            || answer.work_state_confidence
                                < mj_core::activity::verdict::ACT_CONFIDENCE);
                    diagnostic.update(Some("Jev assessed user input need, remaining work, and transient server failures independently."), serde_json::json!({"result": {"work_state":format!("{:?}", answer.work_state), "work_state_confidence":answer.work_state_confidence, "needs_user_input":answer.needs_user_input, "retryable_server_error":answer.retryable_server_error}, "proposed_decision":format!("{:?}", attempt.decision.unwrap())}));
                }
                Err(error) => diagnostic.update(
                    Some("No usable Jev answer."),
                    serde_json::json!({"error":format!("{error:#}")}),
                ),
            }
        }
        match &result {
            Ok(answer) => tracing::info!(target: "mj_jev", request_id = attempt.id, session,
                generation, phase = ?evidence.phase, verdict = ?answer.work_state,
                confidence = %answer.work_state_confidence, needs_user_input = %answer.needs_user_input,
                decision = ?mj_core::activity::verdict::decide(evidence.phase, answer),
                elapsed_ms = attempt.started.elapsed().as_millis() as u64, "Jev classification received"),
            Err(error) => tracing::warn!(target: "mj_jev", request_id = attempt.id, session,
                generation, phase = ?evidence.phase, error = %format!("{error:#}"), "Jev classification failed"),
        }
        (attempt, result)
    }

    pub(crate) async fn ask(&self, evidence: &TurnEvidence) -> Result<TurnVerdict> {
        const MAX_RESPONSE_BYTES: usize = 64 * 1024;
        let request_body = serde_json::to_vec(
            &serde_json::json!({"model":"jev-latest", "state":evidence, "questions":questions()}),
        )
        .context("encode turn evidence")?;
        ensure!(
            request_body.len() <= 64 * 1024,
            "turn evidence exceeds request byte limit"
        );
        let request = match &self.source {
            VerdictSource::Direct { key, endpoint } => self
                .client
                .post(endpoint)
                .bearer_auth(key)
                .json(&self.request_body(evidence)),
            VerdictSource::Hosted { endpoint } => self
                .client
                .post(endpoint)
                .json(&self.request_body(evidence)),
        };
        let mut response = request
            .send()
            .await
            .context("request turn verdict")?
            .error_for_status()
            .context("turn verdict HTTP status")?;
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
        let verdict = TurnVerdict::parse(
            &serde_json::from_slice(&body).context("decode turn verdict JSON")?,
        )?;
        ensure!(
            verdict.retryable_server_error.is_some(),
            "missing server retry verdict"
        );
        Ok(verdict)
    }
}

/// Lives beside the prompt future, so cancellation and shutdown can always win
/// while HTTP is pending. Dropping the turn drops the request and its schedule.
pub(super) async fn await_input_verdict(
    spec: &super::LaunchSpec,
    client: &VerdictClient,
) -> VerdictAttempt {
    await_input_verdict_with_cadence(spec, client, mj_core::activity::SILENCE_WORTH_REPORTING).await
}

async fn await_input_verdict_with_cadence(
    spec: &super::LaunchSpec,
    client: &VerdictClient,
    first: Duration,
) -> VerdictAttempt {
    use mj_core::activity::verdict::{Decision, TurnPhase, decide};
    let mut observed = spec.turn_context.parent_activity();
    let mut gap = first;
    let mut next_silence = first;
    loop {
        let facts = super::turn_stall_facts(spec);
        if observed != spec.turn_context.parent_activity() {
            observed = spec.turn_context.parent_activity();
            gap = first;
            next_silence = first;
        }
        let now = mj_core::clock::epoch_millis();
        let silent = observed.map_or(Duration::ZERO, |at| at.elapsed());
        if silent < next_silence {
            tokio::time::sleep((next_silence - silent).min(Duration::from_secs(1))).await;
            continue;
        }
        let generation = spec.turn_context.generation();
        let mut evidence =
            spec.turn_context
                .evidence(spec.harness, TurnPhase::Running, &facts, now);
        evidence.silent_for_s = silent.as_secs();
        let (mut attempt, answer) = client
            .ask_logged(&spec.turn_context.session_id(), generation, &evidence)
            .await;
        if observed != spec.turn_context.parent_activity()
            || generation != spec.turn_context.generation()
        {
            attempt.finish("discarded", "activity_or_generation_changed");
            continue;
        }
        match answer {
            Ok(verdict) if decide(TurnPhase::Running, &verdict) == Decision::AwaitingInput => {
                return attempt;
            }
            Ok(_) => attempt.finish("unchanged", "keep_current"),
            Err(_) => attempt.finish("unchanged", "request_failed"),
        }
        gap = (gap * 2).min(Duration::from_secs(300));
        next_silence = silent + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::activity::verdict::{TurnPhase, WorkState};
    use mj_transcript::turn_context::TurnContext;
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
            VerdictClient::new(VerdictSource::Direct {
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
    async fn decision_log_records_submitted_body_and_application_without_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let log = mj_core::jev::DecisionLog::open(directory.path().into()).unwrap();
        let (client, server) = server(response("user")).await;
        let client = client.with_log(Some(log));
        let mut evidence = evidence();
        evidence.transcript_summary =
            "User: résumé 🛠\nAssistant: tests remain [earlier history omitted]".into();
        evidence.background_commands = 2;
        let (mut attempt, answer) = client.ask_logged("isolated", 42, &evidence).await;
        assert!(answer.is_ok());
        let submitted = server.await.unwrap();
        let id = attempt.diagnostic.as_ref().unwrap().id();
        attempt.finish("discarded", "activity_or_generation_changed");
        let page = mj_core::jev::read(directory.path(), "isolated", Some(&id)).unwrap();
        assert_eq!(
            page.decisions[0].technical.as_ref().unwrap()["request"],
            submitted
        );
        assert_eq!(page.decisions[0].status, "stale");
        let technical = page.decisions[0].technical.as_ref().unwrap();
        assert_eq!(technical["applied_decision"], serde_json::Value::Null);
        assert_eq!(technical["outcome"], "discarded");
        assert_eq!(technical["reason"], "activity_or_generation_changed");
        let text = serde_json::to_string(&page).unwrap();
        assert!(!text.contains("Bearer"));
        assert!(!text.contains("authorization"));
        assert!(
            mj_core::jev::read(directory.path(), "isolated", None)
                .unwrap()
                .decisions[0]
                .technical
                .is_none()
        );
    }

    #[tokio::test]
    async fn applied_log_records_both_assessments_thresholds_and_actual_decision() {
        let directory = tempfile::tempdir().unwrap();
        let log = mj_core::jev::DecisionLog::open(directory.path().into()).unwrap();
        let (client, server) = server(response("user")).await;
        let client = client.with_log(Some(log));
        let (mut attempt, result) = client.ask_logged("synthetic", 7, &evidence()).await;
        assert_eq!(
            mj_core::activity::verdict::decide(TurnPhase::Running, &result.unwrap()),
            mj_core::activity::verdict::Decision::AwaitingInput
        );
        let id = attempt.diagnostic.as_ref().unwrap().id();
        attempt.finish("applied", "awaiting_input");
        server.await.unwrap();
        let page = mj_core::jev::read(directory.path(), "synthetic", Some(&id)).unwrap();
        let record = &page.decisions[0];
        assert_eq!(record.status, "applied");
        let technical = record.technical.as_ref().unwrap();
        assert_eq!(technical["contract"], "turn-verdict-v4");
        assert_eq!(
            technical["confidence_threshold"],
            serde_json::json!(mj_core::activity::verdict::ACT_CONFIDENCE)
        );
        assert_eq!(
            technical["no_input_threshold"],
            serde_json::json!(mj_core::activity::verdict::NO_INPUT_CONFIDENCE)
        );
        let scores = technical["result"].as_object().unwrap();
        assert_eq!(scores.len(), 4);
        assert_eq!(scores["work_state"], "BackgroundWork");
        assert_eq!(
            scores["work_state_confidence"].as_f64().unwrap() as f32,
            0.95
        );
        assert_eq!(scores["needs_user_input"].as_f64().unwrap() as f32, 0.96);
        assert_eq!(technical["proposed_decision"], "AwaitingInput");
        assert_eq!(technical["applied_decision"], "AwaitingInput");
        assert_eq!(technical["outcome"], "applied");
        assert_eq!(technical["reason"], "awaiting_input");
    }

    #[tokio::test]
    async fn continuous_overall_activity_does_not_postpone_or_invalidate_parent_check() {
        let (client, server) = delayed_server(response("user"), Duration::from_millis(80)).await;
        let spec = super::super::tests::silent_bridge_spec(mj_core::activity::StallPolicy {
            silence: None,
            tool_call: None,
        });
        spec.turn_context
            .reset("Prepare a deployment while the heap task continues independently.");
        spec.turn_context.set_counts(1, 0);
        let activity = spec.acp_activity.clone();
        let updates = tokio::spawn(async move {
            for _ in 0..50 {
                activity.mark();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut attempt = tokio::time::timeout(
            Duration::from_millis(200),
            await_input_verdict_with_cadence(&spec, &client, Duration::from_millis(30)),
        )
        .await
        .unwrap();
        attempt.finish("applied", "awaiting_input");
        let request = server.await.unwrap();
        assert_eq!(request["state"]["background_commands"], 1);
        updates.await.unwrap();
    }

    #[tokio::test]
    async fn changed_inventory_discards_pending_verdict_without_resetting_parent_clock() {
        let (client, server) = delayed_server(response("user"), Duration::from_millis(100)).await;
        let spec = super::super::tests::silent_bridge_spec(mj_core::activity::StallPolicy {
            silence: None,
            tool_call: None,
        });
        spec.turn_context.reset("Approve deployment?");
        spec.turn_context
            .set_background_inventory(vec![], vec!["heap".into()]);
        let context = spec.turn_context.clone();
        let before = context.parent_activity();
        let update = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            context.set_background_inventory(vec![], vec!["replacement".into()]);
            assert_eq!(context.parent_activity(), before);
        });
        assert!(
            tokio::time::timeout(
                Duration::from_millis(200),
                await_input_verdict_with_cadence(&spec, &client, Duration::from_millis(5))
            )
            .await
            .is_err()
        );
        update.await.unwrap();
        server.await.unwrap();
    }

    #[test]
    fn local_keys_choose_direct_and_missing_keys_choose_hosted() {
        assert!(matches!(VerdictSource::for_key(Some("my-key".into())),
            VerdictSource::Direct { key, endpoint } if key == "my-key" && endpoint == TYPESAFE_ENDPOINT));
        assert!(matches!(VerdictSource::for_key(None),
            VerdictSource::Hosted { endpoint } if endpoint == HOSTED_VERDICT_ENDPOINT));
        assert!(
            VerdictClient::resolve_blocking(Some(VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }))
            .is_none()
        );
    }

    /// `[jev] enabled = false` reaches the worker as its launch environment;
    /// the worker then builds no classifier, so no turn evidence is sent and
    /// turns end on the harness's own signals.
    #[test]
    fn the_jev_switch_leaves_the_worker_without_a_classifier() {
        assert!(VerdictClient::resolve_with(None, true).is_none());
        assert!(VerdictClient::resolve_with(None, false).is_some());
    }

    #[tokio::test]
    async fn hosted_requests_send_only_evidence_without_authorization() {
        for status in [200, 429, 502] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v4/turn-verdict", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    assert!(!line.to_ascii_lowercase().starts_with("authorization:"));
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut request = vec![0; length];
                socket.read_exact(&mut request).await.unwrap();
                let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
                assert_eq!(request, serde_json::to_value(evidence()).unwrap());
                let body = response("background_work");
                socket.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let client = VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap();
            let result = client.ask(&evidence()).await;
            assert_eq!(result.is_ok(), status == 200);
            if let Ok(verdict) = result {
                assert_eq!(verdict.work_state, WorkState::BackgroundWork);
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn classifier_sends_bounded_evidence_and_parses_typed_answers() {
        let (client, server) = server(
            serde_json::json!({"answers": {
                "work_state":{"type":"choice","choice":"background_work","confidence":0.95},
                "needs_user_input":{"type":"noul","noul":0.96},
                "retryable_server_error":{"type":"noul","noul":0.01}
            }})
            .to_string(),
        )
        .await;
        let answer = client.ask(&evidence()).await.unwrap();
        assert_eq!(answer.work_state, WorkState::BackgroundWork);
        let request = server.await.unwrap();
        assert_eq!(request["model"], "jev-latest");
        assert_eq!(request["state"]["phase"], "running");
        assert_eq!(request["questions"]["needs_user_input"]["type"], "noul");
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
        let client = VerdictClient::new(VerdictSource::Direct {
            key: "test-key".into(),
            endpoint,
        })
        .unwrap();
        assert!(client.ask(&evidence()).await.is_err());
    }

    fn response(choice: &str) -> String {
        serde_json::json!({"answers": {
            "work_state":{"type":"choice","choice":if choice == "user" { "background_work" } else { choice },"confidence":0.95},
            "needs_user_input":{"type":"noul","noul":if choice == "user" { 0.96 } else { 0.01 }},
            "retryable_server_error":{"type":"noul","noul":0.01}
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
            spec.turn_context.mark_parent_activity();
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
        spec.turn_context.mark_parent_activity();
        let activity = spec.turn_context.clone();
        let update = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            activity.mark_parent_activity();
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

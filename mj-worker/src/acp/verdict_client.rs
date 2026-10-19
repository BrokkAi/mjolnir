//! Optional, bounded TypeSafe turn classification. Failures leave activity alone.
use anyhow::{Context, Result, ensure};
use mj_core::activity::verdict::{
    DirectRequest, TurnEvidence, TurnVerdict, api_key, questions, request_fits,
};
use std::time::Duration;

/// The Unix worker is the only production caller of the classifier client;
/// these items stay compiled on Windows alongside the shared source type.
#[cfg_attr(not(unix), allow(dead_code))]
const HOSTED_VERDICT_ENDPOINT: &str =
    "https://mj-jev-proxy.eng-admin-a63.workers.dev/v7/turn-verdict";
#[cfg_attr(not(unix), allow(dead_code))]
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
    #[cfg_attr(not(unix), allow(dead_code))]
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
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct VerdictClient {
    source: VerdictSource,
    client: reqwest::Client,
    log: Option<mj_core::jev::DecisionLog>,
}

/// Kept alive through application so cancellation has a terminal log event too.
#[cfg_attr(not(unix), allow(dead_code))]
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

#[cfg_attr(not(unix), allow(dead_code))]
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
                (_, "assessment_uncertain") => ("uncertain", "Jev was uncertain; Mj retained the result until relevant evidence changes.".into()),
                (_, "authorized_continuation_ready") => ("deferred", "Authorized unfinished work remains; continuation awaits atomic worker admission.".into()),
                (_, "quota_resolution_required") => ("deferred", "Subscription quota reached; the controller will resolve the provider reset deadline.".into()),
                (_, "automatic_action_suppressed") => ("suppressed", "User input, pause, or a budget limit prevents automatic work.".into()),
                (_, "stale_turn") => ("stale", "A newer turn or user action superseded this assessment.".into()),
                (_, "request_failed") => (
                    "failed",
                    "Jev request failed; mj kept the runtime status.".to_owned(),
                ),
                (_, "structured_request_open") => (
                    "suppressed",
                    "The harness has a question open for the person; mj kept the turn waiting for the answer.".into(),
                ),
                ("discarded", _) => (
                    "stale",
                    "New activity superseded this assessment; mj kept the runtime status."
                        .to_owned(),
                ),
                ("applied", _) => (
                    "applied",
                    if reason == "inferred_finished" {
                        "Mj marked the session ready."
                    } else if reason == "server_retry_armed" {
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
            diagnostic.update(
                None,
                serde_json::json!({
                    "outcome": outcome, "reason": reason,
                    "applied_decision": if outcome == "applied" && reason == "inferred_finished" {
                        Some("InferredFinished".to_owned())
                    } else if outcome == "applied" {
                        self.decision.map(|d| format!("{d:?}"))
                    } else {
                        None
                    }
                }),
            );
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

#[cfg_attr(not(unix), allow(dead_code))]
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
            VerdictSource::Direct { .. } => serde_json::json!(DirectRequest::new(evidence)),
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
        // Completed-turn diagnostics belong to the durable relay owner. A
        // request can finish after its turn was superseded or its action admitted.
        let diagnostic = self
            .log
            .as_ref()
            .filter(|_| evidence.phase == mj_core::activity::verdict::TurnPhase::Running)
            .map(|log| {
                log.start(
                    session,
                    "activity",
                    "Does the running turn require user input?",
                    "Current conversation and live runtime facts.",
                )
            });
        if let Some(diagnostic) = &diagnostic {
            diagnostic.update(None, serde_json::json!({"request":self.request_body(evidence), "contract":"turn-verdict-v7", "questions":questions(), "model":"jev-latest", "confidence_threshold":mj_core::activity::verdict::ACT_CONFIDENCE, "no_input_threshold":mj_core::activity::verdict::NO_INPUT_CONFIDENCE, "required_input_probability":mj_core::assessment::REQUIRED_INPUT_PROBABILITY, "required_input_ratio":mj_core::assessment::REQUIRED_INPUT_RATIO, "finished_work_probability":mj_core::assessment::FINISHED_WORK_PROBABILITY, "stale_failure_probability":mj_core::activity::verdict::STALE_FAILURE_PROBABILITY, "stale_reply_probability":mj_core::activity::verdict::STALE_REPLY_PROBABILITY, "stale_finished_silence_s":mj_core::activity::verdict::STALE_FINISHED_SILENCE.as_secs(), "server_retry_threshold":mj_core::activity::verdict::SERVER_RETRY_CONFIDENCE, "generation":generation, "source":if matches!(self.source, VerdictSource::Direct { .. }) { "direct" } else { "hosted" }}));
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
                    attempt.uncertain = answer
                        .assessment
                        .as_ref()
                        .is_some_and(|v| v.action(false) == mj_core::assessment::Action::Uncertain);
                    diagnostic.update(Some("Jev assessed user input need, remaining work, and transient server failures independently."), serde_json::json!({"result": {"work_state":format!("{:?}", answer.work_state), "work_state_confidence":answer.work_state_confidence, "needs_user_input":answer.needs_user_input, "retryable_server_error":answer.retryable_server_error,"assessment":answer.assessment}, "proposed_decision":format!("{:?}", attempt.decision.unwrap())}));
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
        // Both sources are held to the direct shape: the proxy adds the same
        // questions before it calls TypeSafe.
        ensure!(
            request_fits(evidence),
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
            verdict.assessment.is_some(),
            "missing unified turn assessment"
        );
        Ok(verdict)
    }
}

/// Add the relay owner's authorization history and keep the whole request,
/// questions included, inside the 64 KiB limit. This is shared by running and
/// completed-turn checks, and by evidence stored before the questions grew.
pub(crate) fn bound_authorization(
    evidence: &mut TurnEvidence,
    authorization: Option<mj_core::assessment::ContextHistory>,
) {
    evidence.authorization = authorization;
    // Authorization is historical context. Only the turn owner supplies the
    // current reply; an older question must not become this turn's answer.
    if evidence.authorization.is_some() {
        evidence.transcript_summary.clear();
    }
    if !request_fits(evidence)
        && let Some(mut context) = evidence.authorization.take()
    {
        let mut probe = evidence.clone();
        let fits = context.shrink_until(|candidate| {
            probe.authorization = Some(candidate.clone());
            request_fits(&probe)
        });
        evidence.authorization = fits.then_some(context);
    }
    if !request_fits(evidence) {
        evidence.authorization = None;
        evidence.transcript_summary.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::activity::verdict::{TurnPhase, WorkState};
    use mj_core::assessment::ContextHistory;
    use mj_core::continuation::EvidenceMessage;
    use mj_transcript::turn_context::TurnContext;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    async fn server(body: String) -> (VerdictClient, tokio::task::JoinHandle<serde_json::Value>) {
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
    async fn completed_verdict_sends_authorization_and_applies_shared_wire_budget() {
        let (client, server) = server(response("user")).await;
        let authorization = ContextHistory {
            messages: vec![
                EvidenceMessage {
                    id: "user:task".into(),
                    role: "user".into(),
                    text: "Finish the report and summarize the result.".into(),
                },
                EvidenceMessage {
                    id: "agent:reply".into(),
                    role: "assistant".into(),
                    text: "The report is complete.".into(),
                },
            ],
            ..Default::default()
        };
        let mut completed = evidence();
        completed.phase = TurnPhase::Replied;
        completed.assistant_text_tail = "The report is complete.".into();
        bound_authorization(&mut completed, Some(authorization.clone()));
        let (mut attempt, answer) = client.ask_logged("isolated", 1, &completed).await;
        answer.unwrap();
        attempt.finish("applied", "user_input_required");
        let request = server.await.unwrap();
        assert_eq!(
            request["state"]["authorization"],
            serde_json::to_value(&authorization).unwrap()
        );
        assert_eq!(request["state"]["transcript_summary"], "");
        assert_eq!(
            request["state"]["assistant_text_tail"],
            "The report is complete."
        );

        let mut large = ContextHistory {
            messages: vec![EvidenceMessage {
                id: "user:large".into(),
                role: "user".into(),
                text: "Do the requested work.".into(),
            }],
            ..Default::default()
        };
        large.messages.extend((0..255).map(|index| EvidenceMessage {
            id: format!("{}-{index}", "a".repeat(240)),
            role: "assistant".into(),
            text: "done".into(),
        }));
        let mut bounded = evidence();
        bounded.transcript_summary = "duplicate summary".into();
        bound_authorization(&mut bounded, Some(large));
        let retained = bounded.authorization.as_ref().unwrap();
        assert!(retained.assistant_history_omitted);
        assert_eq!(retained.messages.last().unwrap().role, "assistant");
        assert!(request_fits(&bounded));
        assert!(bounded.transcript_summary.is_empty());

        let oversized_users = ContextHistory {
            messages: vec![EvidenceMessage {
                id: "user:escaped".into(),
                role: "user".into(),
                text: "\n".repeat(32 * 1024),
            }],
            ..Default::default()
        };
        let mut fallback = evidence();
        fallback.transcript_summary = "duplicate summary".into();
        bound_authorization(&mut fallback, Some(oversized_users));
        assert!(fallback.authorization.is_none());
        assert!(fallback.transcript_summary.is_empty());
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
        assert!(!text.contains("fake-key"));
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
        assert_eq!(technical["contract"], "turn-verdict-v7");
        assert_eq!(
            technical["confidence_threshold"],
            serde_json::json!(mj_core::activity::verdict::ACT_CONFIDENCE)
        );
        assert_eq!(
            technical["no_input_threshold"],
            serde_json::json!(mj_core::activity::verdict::NO_INPUT_CONFIDENCE)
        );
        assert_eq!(technical["required_input_probability"], 0.5);
        assert_eq!(technical["required_input_ratio"], 2.5);
        assert_eq!(technical["finished_work_probability"], 0.8);
        assert_eq!(
            technical["result"]["assessment"]["input"]["probabilities"]["required"],
            0.5
        );
        let scores = technical["result"].as_object().unwrap();
        assert_eq!(scores.len(), 5);
        assert_eq!(scores["work_state"], "BackgroundWork");
        assert_eq!(
            scores["work_state_confidence"].as_f64().unwrap() as f32,
            0.95
        );
        assert_eq!(scores["needs_user_input"].as_f64().unwrap() as f32, 0.42);
        assert_eq!(technical["proposed_decision"], "AwaitingInput");
        assert_eq!(technical["applied_decision"], "AwaitingInput");
        assert_eq!(technical["outcome"], "applied");
        assert_eq!(technical["reason"], "awaiting_input");
    }

    #[tokio::test]
    async fn hosted_requests_send_only_evidence_without_authorization() {
        for status in [200, 429, 502] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v7/turn-verdict", listener.local_addr().unwrap());
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
    async fn oversized_classifier_body_is_rejected() {
        let (client, server) = server(" ".repeat(128 * 1024)).await;
        let error = client.ask(&evidence()).await.unwrap_err();
        assert!(error.to_string().contains("exceeds byte limit"));
        server.await.unwrap();
    }

    fn response(choice: &str) -> String {
        let work = if choice == "user" || choice == "background_work" {
            "waiting"
        } else {
            choice
        };
        let input = if choice == "user" { "required" } else { "none" };
        serde_json::json!({"answers": {
            "work": {"type":"choice","choice":work,"confidence":0.95, "probabilities": {"finished": if work == "finished" { 1.0 } else { 0.0 }, "authorized_unfinished": if work == "authorized_unfinished" { 1.0 } else { 0.0 }, "waiting": if work == "waiting" { 1.0 } else { 0.0 }, "unclear": if work == "unclear" { 1.0 } else { 0.0 }}},
            "input": {"type":"choice","choice":input,"confidence":if choice == "user" { 0.42 } else { 0.99 }, "probabilities": {"none": if input == "none" { 1.0 } else { 0.2 }, "redundant_request": if input == "required" { 0.2 } else { 0.0 }, "required": if input == "required" { 0.5 } else { 0.0 }, "unclear": if input == "required" { 0.1 } else { 0.0 }}},
            "failure": {"type":"choice","choice":"none","confidence":0.99, "probabilities": {"none": 1.0, "transient_provider": 0.0, "quota": 0.0, "other": 0.0, "unclear": 0.0}}
        }})
        .to_string()
    }
}
